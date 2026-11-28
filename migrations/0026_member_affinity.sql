-- 为群友人物侧写档案增加好感度与每日防刷限额记录（温和阶梯、暗中运转）。
ALTER TABLE member_profiles ADD COLUMN affinity INTEGER NOT NULL DEFAULT 30;
ALTER TABLE member_profiles ADD COLUMN affinity_daily_day INTEGER NOT NULL DEFAULT 0;
ALTER TABLE member_profiles ADD COLUMN affinity_daily_gain INTEGER NOT NULL DEFAULT 0;
ALTER TABLE member_profiles ADD COLUMN affinity_daily_loss INTEGER NOT NULL DEFAULT 0;
ALTER TABLE member_profiles ADD COLUMN affinity_last_at INTEGER NOT NULL DEFAULT 0;
