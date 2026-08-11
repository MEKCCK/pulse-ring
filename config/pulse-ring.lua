-- pulse-ring Lua 脚本
-- QML 只管静态样式，动态/运动全部由这里控制

local t = 0
local ring_amp = 0

function onUpdate(dt)
    t = t + dt

    -- 整体能量（低通平滑）
    local energy = 0
    for i = 1, 128 do
        energy = energy + (bands[i] or 0)
    end
    energy = energy / 128
    ring_amp = ring_amp * 0.92 + energy * 0.08

    -- 粒子：统一轨道/速度
    config.particleMode = "ring"
    local ps = {}
    for i = 1, 15 do
        ps[i] = {
            x = 0.012,
            angle = (i - 1) * 24,
            speed = 26,
            size = 0.006,
            color = "#D0BCFF",
            life = 60,
            twinkle = 0.15,
        }
    end
    config.particles = ps

    -- 主环运动：幅度跟随能量
    config.growth = 0.14 + ring_amp * 0.12
    config.sensitivity = 1.0 + ring_amp * 0.8
    config.decay = 0.82 + ring_amp * 0.1
    config.smoothness = 0.9 + ring_amp * 0.2

    -- 内/中环运动幅度
    config.innerGrowth = 0.05 + ring_amp * 0.08
    config.midGrowth = 0.06 + ring_amp * 0.08

    -- 自转速度随音乐
    config.autoRotate = 3.0 + ring_amp * 4.0

    -- 空闲呼吸（无音乐时）
    config.idleBreathe = 0.04 + ring_amp * 0.02

    -- 音频条幅度：动态调节
    if t > 0.5 then
        for i = 1, 20 do
            local w = pulse.getWidget(i)
            if w and w.type == "bars" then
                pulse.setWidget(i, "barHeight", 0.04 + energy * 0.16)
            end
        end
    end

    -- 夜间降低亮度
    if time.hour >= 22 or time.hour < 6 then
        config.alpha = 0.6
    else
        config.alpha = 1.0
    end
end

-- 低频增强 + 高频衰减
function transformBands(bands)
    local out = {}
    for i = 1, 128 do
        local v = bands[i]
        if i <= 32 then
            v = v * 1.2
        elseif i >= 96 then
            v = v * 0.85
        end
        out[i] = v
    end
    return out
end

log("pulse-ring lua 已加载")
