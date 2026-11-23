ALTER TABLE spend ADD COLUMN orchestration INTEGER NOT NULL DEFAULT 0
    CHECK (orchestration IN (0, 1));

CREATE TABLE stage_spend_windows (
    root TEXT NOT NULL,
    stage TEXT NOT NULL REFERENCES stage_runs (id) ON DELETE CASCADE,
    opens_at TEXT NOT NULL,
    PRIMARY KEY (root, stage)
) WITHOUT ROWID;
CREATE INDEX stage_spend_windows_by_stage ON stage_spend_windows (stage);
CREATE INDEX stage_spend_windows_by_root ON stage_spend_windows (root, opens_at DESC);

CREATE TABLE stage_spend (
    agent TEXT NOT NULL REFERENCES spend (agent) ON DELETE CASCADE,
    stage TEXT NOT NULL REFERENCES stage_runs (id) ON DELETE CASCADE,
    cost_usd REAL NOT NULL CHECK (cost_usd >= 0),
    estimated_cost_usd REAL NOT NULL CHECK (estimated_cost_usd >= 0),
    PRIMARY KEY (agent, stage)
) WITHOUT ROWID;
CREATE INDEX stage_spend_by_stage ON stage_spend (stage);

-- Only the latest owner of a reused agent can receive new deltas. Leave its
-- existing lifetime total unassigned unless one retained stage owned it.
WITH owners AS (
    SELECT st.id, st.aether_agent AS root, st.started_at,
        (SELECT COUNT(*) FROM stage_runs old
         WHERE old.run = st.run
         AND json_extract(CASE WHEN json_valid(old.data) THEN old.data ELSE '{}' END,
                          '$.aether_agent') = st.aether_agent) AS attempts
    FROM stage_runs st WHERE st.aether_agent IS NOT NULL
)
INSERT INTO stage_spend_windows (root, stage, opens_at)
SELECT root, id, CASE WHEN attempts = 1 THEN '' ELSE started_at END FROM owners;

WITH RECURSIVE scoped(stage, agent) AS (
    SELECT stage, root FROM stage_spend_windows WHERE opens_at = ''
    UNION
    SELECT s.stage, d.agent FROM scoped s JOIN delegated_agents d ON d.parent = s.agent
)
INSERT INTO stage_spend (agent, stage, cost_usd, estimated_cost_usd)
SELECT s.agent, MIN(scoped.stage), s.cost_usd, s.estimated_cost_usd
FROM spend s JOIN scoped ON scoped.agent = s.agent
JOIN stage_runs st ON st.id = scoped.stage JOIN runs r ON r.id = st.run
WHERE s.item = r.item
  AND NOT EXISTS (
      SELECT 1 FROM agent_runs a WHERE a.agent = s.agent AND a.run <> r.id
  )
GROUP BY s.agent HAVING COUNT(*) = 1;

INSERT OR IGNORE INTO agent_runs (agent, run)
SELECT x.agent, st.run FROM stage_spend x JOIN stage_runs st ON st.id = x.stage;

WITH RECURSIVE helpers(agent) AS (
    SELECT value FROM kv WHERE key = 'orchestrator_agent'
    UNION
    SELECT json_extract(j.value, '$.agent') FROM kv,
        json_each(CASE WHEN json_valid(kv.value) THEN kv.value ELSE '[]' END) j
        WHERE kv.key = 'orchestrator_retired' AND j.type = 'object'
    UNION
    SELECT d.agent FROM helpers h JOIN delegated_agents d ON d.parent = h.agent
)
UPDATE spend SET orchestration = 1
WHERE item IS NULL AND agent IN (SELECT agent FROM helpers);
