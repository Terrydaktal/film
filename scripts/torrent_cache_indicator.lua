-- Draw torrent download progress inside mpv's bottombar seekbar track.
local mp = require("mp")
local options = require("mp.options")

local opts = {
    progress_file = "",
}
options.read_options(opts, "torrent_cache_indicator")

local overlay = mp.create_osd_overlay("ass-events")
local last_hover_time = nil
local hide_timeout = 0.5

local function clamp(value, min_value, max_value)
    return math.max(min_value, math.min(value, max_value))
end

local function ass_rect(x1, y1, x2, y2, color, alpha)
    return string.format(
        "{\\an7\\pos(0,0)\\1c&H%s&\\1a&H%s&\\p1}m %d %d l %d %d l %d %d l %d %d{\\p0}",
        color,
        alpha,
        math.floor(x1 + 0.5),
        math.floor(y1 + 0.5),
        math.floor(x2 + 0.5),
        math.floor(y1 + 0.5),
        math.floor(x2 + 0.5),
        math.floor(y2 + 0.5),
        math.floor(x1 + 0.5),
        math.floor(y2 + 0.5)
    )
end

local function osc_canvas(dims)
    local aspect = dims.aspect
    if not aspect or aspect <= 0 then
        aspect = dims.w / dims.h
    end

    local scale_with_window = mp.get_property_bool("osd-scale-by-window", true)
    local playres_y = scale_with_window and 720 or dims.h
    local playres_x = playres_y * aspect

    local pad_x = 9
    local button_w = 27
    local tc_w = 110
    local track_selection_w = 90
    local min_w = (button_w + pad_x) * 5 + (tc_w + pad_x) * 4 + (track_selection_w + pad_x) * 2

    if aspect > 0 and playres_x < min_w then
        playres_y = min_w / aspect
        playres_x = min_w
    end

    return playres_x, playres_y
end

local function seekbar_geometry(dims)
    local playres_x, playres_y = osc_canvas(dims)

    -- Mirrors mpv's built-in OSC bottombar layout constants.
    local pad_x = 9
    local button_w = 27
    local tc_w = 110
    local track_selection_w = 90
    local osc_x = -2
    local osc_h = 56
    local osc_y = playres_y - (osc_h - 2)
    local line_1 = osc_y + 12
    local line_2 = osc_y + 39
    local track_h = osc_h - math.abs(line_2 - line_1)

    local left_timecode_right = osc_x + pad_x + (button_w + pad_x) * 3 + tc_w
    local x1 = left_timecode_right + pad_x

    local fullscreen_left = osc_x + playres_x + 4 - button_w - pad_x
    local volume_left = fullscreen_left - button_w - pad_x
    local subtitle_left = volume_left - track_selection_w - pad_x
    local audio_left = subtitle_left - button_w - pad_x
    local right_timecode_right = audio_left - pad_x - tc_w - 10
    local x2 = right_timecode_right - pad_x

    local min_width = 40
    if x2 - x1 < min_width then
        local center = playres_x / 2
        x1 = center - (min_width / 2)
        x2 = center + (min_width / 2)
    end

    return {
        playres_x = playres_x,
        playres_y = playres_y,
        x1 = clamp(x1, 0, playres_x),
        x2 = clamp(x2, 0, playres_x),
        y1 = line_2 - 2,
        y2 = line_2 + 2,
        hover_y = osc_y - math.max(40, playres_y * 0.25),
    }
end

local function mouse_near_bottombar(dims)
    local visibility = mp.get_property_native("user-data/osc/visibility")
    if visibility == "always" then
        return true
    end
    if visibility == "never" then
        return false
    end

    local mouse = mp.get_property_native("mouse-pos")
    if not mouse or not mouse.hover or not mouse.y then
        return false
    end

    local geo = seekbar_geometry(dims)
    local mouse_y = mouse.y * (geo.playres_y / dims.h)
    return mouse_y >= geo.hover_y
end

local function overlay_should_show(dims)
    if mouse_near_bottombar(dims) then
        last_hover_time = mp.get_time()
        return true
    end

    return last_hover_time ~= nil and (mp.get_time() - last_hover_time) <= hide_timeout
end

local function dirname(path)
    return path:match("^(.*)/[^/]+$") or "."
end

local function progress_file_for(path)
    if opts.progress_file ~= "" then
        return opts.progress_file
    end

    local torrent_dir = path:match("^(.-/stream_cache/[^/]+)")
    if torrent_dir then
        return torrent_dir .. "/.torrent_progress.json"
    end

    return dirname(path) .. "/.torrent_progress.json"
end

local function parse_ranges(content, key)
    local ranges = {}
    local pattern = '"' .. key .. '"%s*:%s*%[(.*)%]'
    local ranges_body = content:match(pattern)
    if not ranges_body then
        return ranges
    end

    for start_s, end_s in ranges_body:gmatch('%[%s*([%d%.]+)%s*,%s*([%d%.]+)%s*%]') do
        local start_b = tonumber(start_s)
        local end_b = tonumber(end_s)
        if start_b and end_b and end_b > start_b then
            ranges[#ranges + 1] = { start_b, end_b }
        end
    end

    return ranges
end

local function torrent_progress(path)
    local progress_path = progress_file_for(path)
    local fh = io.open(progress_path, "r")
    if fh then
        local content = fh:read("*all")
        fh:close()
        local downloaded = tonumber(content:match('"downloaded_bytes"%s*:%s*(%d+)'))
        local total = tonumber(content:match('"total_bytes"%s*:%s*(%d+)'))
        local contiguous_prefix = tonumber(content:match('"contiguous_prefix_bytes"%s*:%s*(%d+)'))
        local playable_prefix = tonumber(content:match('"playable_prefix_ratio"%s*:%s*([%d%.]+)'))
        if downloaded then
            return downloaded, total, parse_ranges(content, "ranges"), contiguous_prefix, playable_prefix
        end
    end

    return nil, nil
end

local function update_overlay()
    local path = mp.get_property("path")
    if (not path or path == "") and opts.progress_file == "" then
        overlay:remove()
        return
    end

    local dims = mp.get_property_native("osd-dimensions")
    if not dims or not dims.w or not dims.h then
        return
    end

    local downloaded, total, ranges, contiguous_prefix, playable_prefix = torrent_progress(path or "")
    if not total or not downloaded or total <= 0 then
        overlay:remove()
        return
    end

    local ratio = math.max(0, math.min(downloaded / total, 1))
    if ratio <= 0 then
        overlay:remove()
        return
    end

    if not overlay_should_show(dims) then
        overlay:remove()
        return
    end

    local geo = seekbar_geometry(dims)
    local track_w = geo.x2 - geo.x1
    local ass = {}

    if ranges and #ranges > 0 then
        local range_cap = 1
        if playable_prefix ~= nil then
            range_cap = clamp(playable_prefix, 0, 1)
        end
        for _, range in ipairs(ranges) do
            local range_start = clamp(range[1] / total, 0, 1)
            local range_end = math.min(clamp(range[2] / total, 0, 1), range_cap)
            if range_end > range_start then
                ass[#ass + 1] = ass_rect(
                    geo.x1 + (track_w * range_start),
                    geo.y1,
                    geo.x1 + (track_w * range_end),
                    geo.y2,
                    "55CC00",
                    "55"
                )
            end
        end
    elseif playable_prefix ~= nil then
        local fill_x2 = geo.x1 + (track_w * clamp(playable_prefix, 0, 1))
        if fill_x2 > geo.x1 then
            ass[#ass + 1] = ass_rect(geo.x1, geo.y1, fill_x2, geo.y2, "55CC00", "55")
        end
    elseif contiguous_prefix and contiguous_prefix > 0 then
        local fill_x2 = geo.x1 + (track_w * clamp(contiguous_prefix / total, 0, 1))
        ass[#ass + 1] = ass_rect(geo.x1, geo.y1, fill_x2, geo.y2, "55CC00", "55")
    else
        local fill_x2 = geo.x1 + (track_w * ratio)
        ass[#ass + 1] = ass_rect(geo.x1, geo.y1, fill_x2, geo.y2, "55CC00", "55")
    end

    overlay.res_x = geo.playres_x
    overlay.res_y = geo.playres_y
    overlay.data = table.concat(ass, "\n")
    overlay:update()
end

mp.observe_property("osd-dimensions", "native", update_overlay)
mp.observe_property("mouse-pos", "native", update_overlay)
mp.observe_property("path", "string", update_overlay)
mp.add_periodic_timer(0.1, update_overlay)
