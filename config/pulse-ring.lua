-- pulse-ring Lua 脚本
-- QML 只管静态样式，动态/运动全部由这里控制

local t = 0
local ring_amp = 0
local prev_sens = 1.0
local prev_growth = 0.14

function onUpdate(dt)
    t = t + dt

    -- 整体能量（强低通，抑制抖动）
    local energy = 0
    for i = 1, 128 do
        energy = energy + (bands[i] or 0)
    end
    energy = energy / 128
    ring_amp = ring_amp * 0.97 + energy * 0.03

    -- 粒子：统一轨道/速度（环绕星环）
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

    -- 主环运动：温和跟随
    local target_growth = 0.14 + ring_amp * 0.06
    local target_sens = 1.6 + ring_amp * 0.4
    prev_growth = prev_growth * 0.9 + target_growth * 0.1
    prev_sens = prev_sens * 0.9 + target_sens * 0.1
    config.growth = prev_growth
    config.sensitivity = prev_sens
    config.decay = 0.86
    config.smoothness = 1.0

    -- 内/中环运动（温和）
    config.innerGrowth = 0.05 + ring_amp * 0.05
    config.midGrowth = 0.06 + ring_amp * 0.05

    -- 自转速度（温和）
    config.autoRotate = 3.0 + ring_amp * 2.0

    -- 空闲呼吸
    config.idleBreathe = 0.04 + ring_amp * 0.02

    -- 音频条幅度（温和）
    if t > 0.5 then
        for i = 1, 20 do
            local w = pulse.getWidget(i)
            if w and w.type == "bars" then
                pulse.setWidget(i, "barHeight", 0.05 + ring_amp * 0.10)
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

-- 频段变换：中性（平滑处理交给引擎，灵敏度由 sensitivity 统一控制）
function transformBands(bands)
    return bands
end

log("pulse-ring lua 已加载")
