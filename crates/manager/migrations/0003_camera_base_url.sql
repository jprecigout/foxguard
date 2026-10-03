-- L'URL de base d'une caméra décrit la CAMÉRA, pas le clip d'un événement.
--
-- Elle vivait dans `clip_base_url`, aux côtés du nom de fichier du clip. Mais
-- l'interface s'en sert pour deux choses — ouvrir le clip d'un événement, et
-- ouvrir le DIRECT de la caméra — et la seconde ne doit pas dépendre de
-- l'existence de la première : une caméra dont aucune détection n'a produit de
-- clip se regarde quand même.
ALTER TABLE detection_events
    ADD COLUMN IF NOT EXISTS base_url TEXT;

-- Les événements déjà enregistrés gardent leur lien : la colonne d'origine
-- portait la même valeur, simplement limitée aux événements munis d'un clip.
UPDATE detection_events
   SET base_url = clip_base_url
 WHERE base_url IS NULL
   AND clip_base_url IS NOT NULL;

-- `clip_base_url` n'a plus de raison d'être : sa valeur est reprise ci-dessus,
-- et la garder laisserait deux sources pour une même information — celle que
-- le code lit, et celle qu'il oublie de mettre à jour.
ALTER TABLE detection_events
    DROP COLUMN IF EXISTS clip_base_url;
