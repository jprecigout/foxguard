-- Média associé à un événement de détection : la vignette qui rend la
-- timeline de l'interface lisible d'un coup d'œil, et de quoi retrouver le
-- clip vidéo sur la caméra qui l'a écrit.
--
-- Migration ADDITIVE, en colonnes toutes nullables : les lignes déjà en base
-- n'ont pas de média et n'en auront jamais, et une caméra antérieure à
-- l'introduction de ces champs continue d'être enregistrée telle quelle (voir
-- la règle de compatibilité du crate `foxguard-protocol`).
ALTER TABLE detection_events
    -- La vignette elle-même, en octets JPEG.
    --
    -- Stockée EN BASE plutôt que référencée sur la caméra : la timeline doit
    -- rester consultable des mois plus tard et depuis n'importe où, alors que
    -- la caméra n'est joignable que depuis son réseau local et purge ses
    -- fichiers au bout de quelques jours. À quelques kilo-octets par
    -- changement d'état — et non par frame — le volume reste modeste, et la
    -- purge des événements emporte les vignettes avec elle.
    --
    -- PostgreSQL met automatiquement ces valeurs de côté (stockage TOAST) :
    -- elles ne pèsent donc pas sur les requêtes de liste, qui ne les lisent
    -- pas.
    ADD COLUMN IF NOT EXISTS thumbnail BYTEA,

    -- Nom du fichier de clip sur la caméra, tel qu'attendu par sa route
    -- `GET /recordings/{file}`.
    ADD COLUMN IF NOT EXISTS clip_file TEXT,

    -- URL de base par laquelle cette caméra était joignable AU MOMENT de
    -- l'événement.
    --
    -- Conservée par événement, et non dans une table de caméras : c'est une
    -- donnée historique. Une caméra qui change d'adresse ne doit pas rendre
    -- faux les liens de tous ses événements passés — ils seraient de toute
    -- façon invalides, mais pour la bonne raison.
    ADD COLUMN IF NOT EXISTS clip_base_url TEXT;
