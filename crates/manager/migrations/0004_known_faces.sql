-- Photos de référence de la reconnaissance faciale, telles que les caméras
-- les partagent sur MQTT (voir `foxguard_protocol::faces`).
--
-- Une COPIE pour l'interface : la source de vérité reste le broker, qui garde
-- chaque photo en message retenu et la remet à toute caméra qui s'abonne. Le
-- manager ne s'en sert que pour montrer qui est enregistré, et avec quelles
-- photos ; il n'en renvoie aucune aux caméras.
--
-- Pas de purge : contrairement aux événements, une photo de référence ne
-- vieillit pas. Elle disparaît quand une caméra publie sa suppression.
CREATE TABLE IF NOT EXISTS known_faces (
    -- Nom du fichier dans `known_faces/`, identique sur toutes les caméras :
    -- c'est l'identifiant de la photo sur le fil comme ici.
    file        TEXT        PRIMARY KEY,
    -- Déduit du nom de fichier, avec la même règle que les caméras (voir
    -- `foxguard_protocol::faces::person_name`). Stocké plutôt que recalculé
    -- pour regrouper et trier en SQL.
    person_name TEXT        NOT NULL,
    -- Caméra qui a pris (ou partagé) la photo.
    camera      TEXT        NOT NULL,
    captured_at TIMESTAMPTZ NOT NULL,
    -- Les octets du fichier (JPEG, ou PNG pour une photo déposée à la main).
    -- Mis de côté par PostgreSQL (TOAST) : la liste ne les lit pas.
    photo       BYTEA       NOT NULL,
    received_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- L'interface liste les photos par personne, de la plus ancienne à la plus
-- récente.
CREATE INDEX IF NOT EXISTS known_faces_person_name_idx
    ON known_faces (person_name, captured_at);
