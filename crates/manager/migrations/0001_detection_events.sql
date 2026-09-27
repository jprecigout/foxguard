-- Événements de détection reçus des caméras via MQTT.
--
-- Le format de fil est défini par le crate `foxguard-protocol` ; cette table
-- en est la projection relationnelle. Le statut y est décomposé en deux
-- colonnes (`status` + `person_name`) plutôt qu'en un type énuméré : ajouter
-- un statut à l'avenir ne demandera alors pas de migration de type, et une
-- caméra plus récente qui en émettrait un inconnu ne ferait pas échouer
-- l'insertion (voir la règle de compatibilité du crate protocol).
CREATE TABLE IF NOT EXISTS detection_events (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    camera      TEXT        NOT NULL,
    occurred_at TIMESTAMPTZ NOT NULL,
    status      TEXT        NOT NULL,
    person_name TEXT,
    -- Date de réception par le manager, distincte de `occurred_at` : l'écart
    -- entre les deux révèle une horloge de caméra déréglée ou un broker qui
    -- a retenu des messages.
    received_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- L'interface liste les événements du plus récent au plus ancien : c'est la
-- requête chaude, elle doit être servie par l'index.
CREATE INDEX IF NOT EXISTS detection_events_occurred_at_idx
    ON detection_events (occurred_at DESC);

-- Filtrage et recensement par caméra.
CREATE INDEX IF NOT EXISTS detection_events_camera_occurred_at_idx
    ON detection_events (camera, occurred_at DESC);
