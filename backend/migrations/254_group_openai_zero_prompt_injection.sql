-- 2026-11-26: Opt-in per-group prompt policy; existing groups retain their behavior.
ALTER TABLE groups
    ADD COLUMN IF NOT EXISTS openai_zero_prompt_injection BOOLEAN NOT NULL DEFAULT FALSE;

COMMENT ON COLUMN groups.openai_zero_prompt_injection IS
    'Disable gateway-generated prompt text for requests in this OpenAI group while preserving client content';
