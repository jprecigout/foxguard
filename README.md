# <img src="crates/camera/assets/logo.svg" width="40" height="40" alt="Logo FoxGuard"> FoxGuard

**FoxGuard** est un système de vidéosurveillance intelligent propulsé par l'IA et développé en Rust.

---

## 🚀 Fonctionnalités Principales

* **Détection d'objets par IA** : Analyse des flux vidéo en temps réel à l'aide d'un modèle YOLOv8 (via `tract-onnx`), restreint aux classes personne / chat / chien.
* **Tracking des personnes** : Suivi de chaque personne détectée d'une frame à l'autre (association par IoU), pour ne relancer la reconnaissance faciale que lorsque c'est nécessaire (nouvelle personne, déplacement significatif, ou périodiquement).
* **Reconnaissance faciale** : Détection et alignement du visage (YuNet, recadré en 112x112) puis extraction d'une empreinte faciale (ArcFace / MobileFaceNet), comparée par similarité cosinus à une base de visages connus.
* **Capture de photo de référence depuis l'interface web** : Un bouton « Capturer » enregistre la frame webcam courante comme nouveau gabarit de référence pour un nom donné. Plusieurs captures (angles/poses différents) s'accumulent pour la même personne au lieu de se remplacer, ce qui rend la reconnaissance plus fiable (voir `known_faces/`).
* **Interface Web de Contrôle** : Panneau de contrôle moderne intégré et servi via le framework web Axum (`crates/camera/static/controller.html`).
* **Streaming Vidéo en Direct** : Diffusion par WebSocket (`/ws`) en **H.264 décodé par le navigateur** (WebCodecs) et dessiné dans un `<canvas>`. C'est le SEUL format vidéo de la caméra : **1651 kb/s en MJPEG contre 76 kb/s en H.264** sur la même scène, 21 fois moins. Encodage logiciel (openh264), et **aucune frame n'est encodée tant que personne ne regarde, qu'aucun enregistrement n'est en cours et que la surveillance est éteinte**.
* **Flux RTSP (optionnel)** : Le même flux, servi en RTSP (`rtsp://<caméra>:8554/stream?token=...`), lisible par n'importe quel lecteur ou enregistreur vidéo (VLC, ffmpeg, Home Assistant, Frigate, un NVR). Seul le SERVEUR est optionnel : l'encodage, lui, a lieu de toute façon pour les interfaces web et les enregistrements (voir `[rtsp]`).
* **Pré-filtre de mouvement** : YOLO n'est relancé que si l'image a réellement changé. Une caméra de surveillance regarde une scène immobile l'essentiel du temps, et l'inférence est de loin le poste de dépense dominant sur un Raspberry Pi ; comparer deux miniatures coûte quatre ordres de grandeur de moins. Deux garde-fous évitent que cette économie se paie en détections manquées (voir `[motion]`).
* **Clips d'événement** : Quelques secondes de MP4 autour de chaque détection, **pré-enregistrement compris**, écrites dans le dossier des enregistrements et soumises à la même rétention. C'est ce qui donne à la timeline de l'interface du manager un accès direct à ce qui s'est passé.
* **Gestion des enregistrements** : Suppression manuelle depuis l'interface web (bouton par enregistrement, avec confirmation), et purge automatique des enregistrements dépassant la durée de conservation configurée (voir `[recording]`).
* **Enregistrement Vidéo** : Sauvegarde en **MP4 fragmenté** (H.264) — une vingtaine de fois plus léger que le format d'images JPEG qui l'a précédé, lisible par un `<video>` de navigateur comme par VLC, et *résistant à la troncature* : une coupure de courant n'y coûte que le dernier fragment, là où un MP4 ordinaire serait intégralement perdu. Activable dynamiquement depuis l'interface web, avec liste et relecture directement dans l'UI.
* **Alertes par E-mail** : Notification HTML automatique (avec logo et photo de la détection en pièces jointes inline) en cas de détection, avec gestion de délai (*cooldown*) pour éviter le spam.
* **Publication MQTT (optionnelle)** : Publie un message JSON sur un broker MQTT à chaque *changement* d'état de reconnaissance d'une personne suivie (nouvelle personne inconnue, ou identification/perte d'identification), avec le nom de la caméra, l'horodatage, le nom de la personne si elle est connue, une **vignette** de la détection et la référence du **clip** correspondant. Désactivée par défaut, activable via `[mqtt] enabled = true` (voir Configuration ci-dessous).
* **Timeline des détections (interface du manager)** : Pour chaque caméra, une bande de 24 heures qui situe les détections dans la journée, et une pellicule de vignettes qui montre ce qui s'est passé. Un clic ouvre le clip correspondant — et un bouton donne accès au **direct** de la caméra, servi par elle et affiché dans un cadre.
* **Overlay Graphique Natif** : Dessin de boîtes englobantes (*bounding boxes*) et de libellés textuels optimisés grâce à la police bitmap `font8x8`.

---

## 🏗️ Architecture du dépôt

FoxGuard est un **monorepo** regroupant trois composants déployés sur des
machines différentes.

| Composant | Emplacement | Tourne sur | Rôle |
| --- | --- | --- | --- |
| `foxguard-camera` | `crates/camera/` | **Raspberry Pi**, près de la caméra | Capture V4L2, détection YOLO, reconnaissance faciale, alertes, enregistrement |
| `foxguard-manager` | `crates/manager/` | **Serveur annexe** | Agrège les événements de plusieurs caméras, expose une API HTTP |
| `foxguard-protocol` | `crates/protocol/` | *(bibliothèque partagée)* | Le format des messages qui transitent sur MQTT |
| `foxguard-ui` | `ui/` | **Serveur annexe** | Interface React : timeline des détections de chaque caméra, avec vignettes et accès aux clips |

```
caméra (Raspberry Pi) ──MQTT──▶ broker ──▶ manager (serveur) ──HTTP──▶ ui
   crates/camera                          crates/manager              ui/
        └──────────── crates/protocol ────────────┘
```

### Pourquoi un monorepo

`foxguard-protocol` est le **contrat de fil** entre la caméra et le manager.
Dans des dépôts séparés, le manager redéclarerait les structures à la main :
le jour où un champ change, ça compilerait des deux côtés et ça casserait
silencieusement à l'exécution. Ici, rompre le contrat est une erreur de
**compilation**.

### Compatibilité entre versions déployées

Le crate partagé garantit la cohérence des *sources*, pas celle des *binaires
déployés* : caméra et manager étant mis à jour indépendamment, une caméra en
v1.2 parlera tôt ou tard à un manager en v1.4. Toute évolution de
`DetectionEvent` doit donc rester rétrocompatible — champs nouveaux toujours
optionnels, aucun champ existant renommé ni supprimé. C'est la discipline déjà
appliquée au fichier de configuration, transposée au fil MQTT.

### Compilation croisée ARM64

Le `Cargo.lock` est commun, mais **la compilation ne l'est pas** :
`cargo build -p foxguard-camera` ne construit que le sous-graphe de
dépendances de la caméra. Les dépendances propres au manager (et leur
éventuel `ring`, dont l'assembleur par architecture a déjà fait échouer la
compilation croisée sous QEMU) apparaissent dans le lock sans jamais être
compilées pour le Raspberry Pi.

### Commandes utiles

```bash
cargo test --workspace                  # toute la suite (et régénère ui/src/generated/)
cargo run -p foxguard-camera            # caméra (depuis la RACINE du dépôt)
cargo run -p foxguard-manager           # manager
cargo clippy --workspace --all-targets  # analyse statique
```

Deux parties de la suite s'**ignorent d'elles-mêmes** quand leur dépendance
externe est absente, pour qu'un `cargo test` reste vert sur n'importe quel
poste — pensez donc à les armer en CI, sans quoi elles ne vérifient rien :

| Tests | Dépendance | Comment les armer |
| --- | --- | --- |
| `crates/manager/tests/{postgres,api_postgres}.rs` | PostgreSQL | `FOXGUARD_TEST_DATABASE_URL=…` (voir « Tests » du manager) |
| `crates/camera/tests/rtsp_gstreamer.rs` | GStreamer | installer `gst-launch-1.0` et ses greffons H.264 |

Le second fait décoder le flux RTSP par un lecteur INDÉPENDANT. C'est la seule
vérification qui ait valeur de preuve : un flux peut satisfaire toutes nos
propres assertions et rester indécodable pour VLC.

⚠️ La caméra se lance **depuis la racine du workspace** : les chemins de
`camera-config.toml` (modèles ONNX, dossiers de données) sont relatifs au répertoire
de travail.

---

## 📂 Structure du Projet

* **`crates/camera/src/main.rs`** : Point d'entrée de l'application — bannière de démarrage, chargement de `camera-config.toml`, initialisation de l'état partagé, lancement de la boucle caméra (tâche bloquante) et du serveur Axum.
* **`crates/camera/src/capture/`** : Capture caméra (V4L2) et pipeline de traitement, découpé par responsabilité (chaque étape a son propre fichier ; c'est `mod.rs` qui les enchaîne).
  * **`mod.rs`** : `start_camera_loop` — ouverture du périphérique V4L2, chargement des modèles et de la base de visages connus, démarrage du worker de reconnaissance, puis boucle de capture.
  * **`state.rs`** : `SharedState`, état partagé avec le serveur HTTP/WebSocket (surveillance/enregistrement actifs, jeton API, canal de diffusion, capture de référence en attente).
  * **`models.rs`** : Chargement des modèles IA (YOLO, YuNet, ArcFace) et de la base de visages connus au démarrage (`Models`).
  * **`tracking.rs`** : Suivi des personnes d'une frame à l'autre par IoU (`PersonTracker`) et reconnaissance faciale parallèle (YuNet + ArcFace, parallélisée avec Rayon). Détecte aussi, sans effet de bord, les *changements* d'état de reconnaissance de chaque personne suivie (inconnue ↔ identifiée), pour piloter la publication MQTT (voir `worker.rs` et `../mqtt.rs`).
  * **`known_faces.rs`** : Chargement (parallélisé avec Rayon) et rechargement à chaud du dossier `known_faces/`, et capture de photo de référence depuis l'UI web (rechargement en tâche de fond via `tokio::task::spawn_blocking`).
  * **`worker.rs`** : Thread d'arrière-plan (`tokio::task::spawn_blocking`) qui exécute le pipeline YOLO → tracking → reconnaissance, et publie sur MQTT (si activé) les changements d'état retournés par `tracking.rs`.
  * **`overlay.rs`** : Incrustation des bounding-box et de leur légende sur la frame vidéo.
  * **`motion.rs`** : Pré-filtre de mouvement — décide, en comparant deux miniatures de l'image, si l'inférence YOLO vaut la peine d'être lancée. Porte les deux garde-fous (rémanence après mouvement, passage périodique de sécurité) qui évitent que cette économie coûte des détections.
  * **`thumbnail.rs`** : Vignette d'une détection, recadrée sur la personne, embarquée dans l'événement MQTT.
  * **`clips.rs`** : Clips vidéo d'événement — tampon circulaire de pré-enregistrement et écriture du clip autour de chaque détection.
  * **`codec.rs`** : Décodage YUYV → RGB, parallélisé avec Rayon.
  * **`recording.rs`** : Écriture des enregistrements et des clips en MP4 fragmenté, avec une durée réelle par image.
  * **`capture_loop.rs`** : Boucle principale de lecture V4L2 — incrustation des boîtes, alerte e-mail, alimentation des clips, encodage H.264, enregistrement disque et diffusion WebSocket. Rien n'y est encodé tant que personne n'en a l'usage, et la frame n'y est décodée **qu'une fois et que si quelqu'un en a besoin** — sur un Raspberry Pi, qui fournit du YUYV, le chemin nominal ne la décode donc pas du tout. C'est aussi là que la **cadence de la reconnaissance** est décidée (250 ms), avant la copie de la frame : le worker ne reçoit que les frames qu'il va réellement analyser.
* **`crates/camera/src/api.rs`** : Routage Axum, page de contrôle (`GET /`), vue en direct seule (`GET /live`), interrupteur de surveillance seul (`GET /control`) et lecteur d'un enregistrement (`GET /play/{filename}`), WebSocket vidéo H.264 authentifié par jeton (`GET /ws?token=...`), qui porte la vidéo dans un sens et les commandes JSON dans l'autre (`set_monitoring`, `set_detection`, `set_recording`, `capture_reference`), état et pilotage de la surveillance en HTTP (`GET /api/monitoring`, `POST /api/monitoring?token=...`, voir « Le pilotage depuis le manager »), liste (`GET /api/recordings`), téléchargement (`GET /recordings/{filename}`, délégué à `ServeFile` donc servi par plages d'octets, ce dont un `<video>` a besoin pour se déplacer dans un MP4) et suppression (`DELETE /api/recordings/{filename}?token=...`, authentifiée par le même jeton que le WebSocket) des enregistrements.
* **`crates/camera/src/h264/`** : Encodage H.264 du flux (openh264), sans aucune connaissance du réseau.
  * **`i420.rs`** : Conversion vers le format d'entrée de l'encodeur, depuis le YUYV brut de la caméra (chemin le moins coûteux) ou depuis une image RGB déjà incrustée des boîtes de détection.
  * **`encoder.rs`** : L'encodeur lui-même, les unités d'accès produites et les jeux de paramètres (SPS/PPS).
  * **`stream.rs`** : Le flux partagé entre la boucle de capture (qui produit) et ses trois consommateurs — RTSP, WebSocket des interfaces, enregistrements. C'est lui qui sait si quelqu'un regarde, et donc s'il faut encoder.
* **`crates/camera/src/mp4/`** : Écriture de MP4 fragmentés contenant ce flux.
  * **`boxes.rs`** : Les briques du format ISO-BMFF — une boîte déclare sa taille avant son contenu, donc on écrit d'abord et on renseigne ensuite.
  * **`writer.rs`** : Le segment d'initialisation et les fragments, un par groupe d'images.
* **`crates/camera/src/rtsp/`** : Serveur RTSP qui distribue ce flux aux lecteurs du réseau.
  * **`server.rs`** : Acceptation des lecteurs et dialogue RTSP (`OPTIONS`, `DESCRIBE`, `SETUP`, `PLAY`, `TEARDOWN`).
  * **`message.rs`** : Analyse des messages RTSP, y compris les paquets binaires entrelacés que les lecteurs renvoient sur la même connexion.
  * **`transport.rs`** : Négociation du transport (RTP dans la connexion TCP, ou en UDP).
  * **`sdp.rs`** : Description du flux renvoyée à un `DESCRIBE`.
  * **`rtp.rs`** : Empaquetage des frames en paquets RTP (RFC 3550 / 6184, fragmentation FU-A comprise) et rapports RTCP.
* **`crates/camera/src/vision/`** : Pipeline de vision par ordinateur, un fichier par étape.
  * **`object_detector.rs`** : `ObjectDetector` (YOLOv8), restreint aux classes personne / chat / chien. La frame y est mise en **letterbox** (bandes grises) et non écrasée vers le carré d'entrée du modèle : YOLOv8 a été entraîné ainsi, et déformer le 4:3 de la caméra lui présente des silhouettes qu'il n'a jamais vues.
  * **`face_detector.rs`** : `FaceDetectorYuNet`, détection et alignement de visage (recadré en 112x112).
  * **`face_recognition.rs`** : `FaceEmbedder`, empreinte ArcFace / MobileFaceNet et comparaison des identités par similarité cosinus.
  * **`model.rs`** : Chargement ONNX mutualisé par les trois modèles ci-dessus.
  * **`types.rs`** : Types partagés du pipeline (`BoundingBox`, `KnownPerson`).
* **`crates/camera/models/`** : Les 3 modèles ONNX embarqués (`yolov8n.onnx`, `face_detection_yunet_2023mar.onnx`, `arcface-mobilefacenet.onnx`).
* **`crates/camera/src/geometry.rs`** : Calcul d'intersection sur union (IoU), utilitaire partagé entre le tracking (`capture/tracking.rs`) et la détection d'objets/visages (`vision/object_detector.rs`, `vision/face_detector.rs`), pour éviter de dupliquer ce calcul.
* **`crates/camera/src/config.rs`** : Chargement et structures de `camera-config.toml` (serveur, caméra, détection, e-mail, MQTT, enregistrements et clips, mouvement, réglages de l'encodage H.264, serveur RTSP).
* **`crates/camera/src/mail.rs`** : Construction et envoi des alertes e-mail (HTML multipart avec logo et photo de la détection).
* **`crates/camera/src/retention.rs`** : Tâche de fond qui supprime les enregistrements dépassant la durée de conservation configurée (`[recording] retention_days`), et prédicat partagé `is_recording_file` qui définit ce qui est un enregistrement pour la liste, le téléchargement, la suppression et la purge.
* **`crates/camera/src/mqtt.rs`** : Connexion à un broker MQTT et publication des événements de détection (nom de caméra, horodatage, statut connu/inconnu) à chaque changement d'état ; fonctionnalité optionnelle (voir `[mqtt]` dans `camera-config.toml`).
* **`crates/camera/src/util.rs`** : Petits utilitaires transverses (verrouillage de mutex tolérant à l'empoisonnement).
* **`crates/camera/static/controller.html`** : Interface utilisateur web — flux vidéo H.264 décodé par WebCodecs, interrupteurs, capture de photo de référence, liste et lecture des enregistrements (un `<video>` natif, les enregistrements étant des MP4).
* **`crates/camera/static/clip-player.html`** : Lecteur autonome d'un enregistrement, servi par `GET /play/{fichier}`. Un `<video>` natif suffit, les enregistrements étant des MP4 ordinaires.
* **`crates/camera/static/live.html`** : Vue en direct SEULE, servie par `GET /live`. C'est par elle que l'interface du manager affiche le direct d'une caméra, sans jamais recevoir son jeton d'API (voir « La timeline, les clips et le direct » plus bas).
* **`crates/camera/static/control.html`** : Interrupteur de surveillance SEUL, servi par `GET /control`. Même mécanique que `live.html`, et pour la même raison : c'est par elle que l'interface du manager active ou coupe la surveillance d'une caméra, sans jamais recevoir son jeton (voir « Le pilotage depuis le manager » plus bas). Elle passe par `GET`/`POST /api/monitoring` et **n'ouvre pas le WebSocket** : s'y abonner démarrerait l'encodage H.264 pour une page qui n'affiche aucune image.
* **`crates/camera/assets/logo.svg`** : Logo FoxGuard — affiché dans ce README et embarqué dans le binaire (`include_bytes!`) pour les e-mails d'alerte.
* **`known_faces/`** : Photos de référence pour la reconnaissance faciale, nommées `<nom>_<horodatage>.jpg` (plusieurs fichiers possibles par personne).
* **`output_record/`** : Enregistrements vidéo générés par l'application, en MP4 fragmenté (voir « Les enregistrements »).
* **`crates/manager/`** : Le manager — `config.rs` (sa configuration `manager-config.toml`), `ingest.rs` (abonnement MQTT et décodage des événements), `db.rs` (persistance PostgreSQL), `retention.rs` (purge des événements trop anciens), `api.rs` (API HTTP de consultation et service du bundle de l'interface), `migrations/` (schéma, appliqué au démarrage).
* **`crates/protocol/`** : `DetectionEvent` et `PersonStatus`, le contrat partagé entre la caméra et le manager.
* **`ui/`** : Interface React du manager — `App.tsx` (composition), `components/` (barre de jour, timeline d'une caméra, cadre de lecture d'un clip ou du direct), `timeline.ts` (géométrie temporelle de la bande de 24 h), `grouping.ts` (regroupement par caméra), `generated/` (types d'API générés depuis le Rust). Voir `ui/README.md`.
* **`deploy/camera/`** : `Dockerfile` de l'image Raspberry Pi.
* **`deploy/server/`** : `Dockerfile` du manager, `compose.yml` (manager + broker MQTT) et `mosquitto.conf`.

---

## 🎬 L'encodage H.264 (`[h264]`)

Le H.264 est le **seul chemin vidéo** de la caméra. Il n'y a donc pas
d'interrupteur dans `[h264]`, seulement des réglages : désactiver l'encodage
ne laisserait aucun flux à diffuser ni aucun format à enregistrer.

Le flux encodé sert **trois consommateurs**, et n'est encodé qu'une fois pour
les trois :

| Consommateur | Ce qu'il en fait |
| --- | --- |
| Les **interfaces web** | Le direct, décodé par le navigateur lui-même (WebCodecs) et dessiné dans un `<canvas>` |
| Les **enregistrements** | Des MP4, une vingtaine de fois plus légers que le format historique |
| Le **serveur RTSP** | Le flux que lisent VLC, ffmpeg, un NVR, une domotique |

Mesuré sur une scène de bureau en 640x360 : **1651 kb/s en MJPEG contre 76
kb/s en H.264**, soit 21 fois moins.

### Mettre à jour une configuration existante

`[h264] enabled` **n'existe plus**. Un `camera-config.toml` antérieur reste
valide — les clés inconnues sont ignorées — mais une installation qui avait
`enabled = false` se met à encoder après la mise à jour, en silence. C'est la
conséquence directe du passage à un format unique, et la seule rupture de la
discipline de compatibilité que ce dépôt s'impose par ailleurs sur sa
configuration. La ligne peut être supprimée, elle ne sert plus à rien.

`[rtsp]` n'est pas touché : un flux RTSP activé le reste.

### Pourquoi les interfaces web ne « lisent pas le RTSP »

**Aucun navigateur n'implémente RTSP.** Le flux RTSP s'adresse aux lecteurs
vidéo du réseau, pas aux pages web. Pour afficher du H.264 dans une page, il
faut le lui apporter par un transport qu'elle connaît et le faire décoder par
elle : les unités d'accès partent donc sur un **WebSocket** (`/ws`), et
`VideoDecoder` (WebCodecs) les décode côté navigateur.

C'est la voie la moins coûteuse des trois possibles : pas de conteneur à
écrire (contrairement au MP4 fragmenté qu'exigerait un `<video>` en direct), et
pas de pile WebRTC — dont les dépendances cryptographiques sont précisément ce
que la compilation croisée ARM64 de ce dépôt s'applique à éviter.

### Le prérequis, puisqu'il n'y a plus de repli

L'affichage du direct exige donc **WebCodecs** : Chrome/Edge 94+, Safari 16.4+,
Firefox 130+. Un flux MJPEG a longtemps servi de repli ; il a été retiré, et
c'est un arbitrage assumé plutôt qu'un oubli.

Ce qu'on perd : un navigateur plus ancien ne voit plus le direct. Les deux
interfaces le **disent** explicitement dans ce cas, au lieu de laisser un
canevas noir faire croire à une caméra en panne — et tout le reste continue de
fonctionner, enregistrements compris, puisque ce sont des MP4 ordinaires.

Ce qu'on gagne : un seul chemin vidéo. Deux formats signifiaient deux
WebSockets, deux lecteurs dans chaque page, deux écrivains d'enregistrement,
une route de capacités pour choisir entre les deux, et une bascule à tester
dans les deux sens. Chacun de ces embranchements était un endroit où les deux
chemins pouvaient diverger sans qu'on le voie.

### Ce que ça coûte, et ce qui le borne

L'encodage est **logiciel** (openh264). C'est la dépense la plus lourde qu'on
puisse ajouter à un Raspberry Pi, d'où deux garde-fous :

1. **aucune frame n'est encodée tant que personne ne regarde, qu'aucun
   enregistrement n'est en cours et que la surveillance est éteinte** — le
   flux sait s'il a des abonnés, et la boucle de capture ne l'alimente que
   dans ce cas ;
2. la cadence du flux (`fps`, 12 par défaut) est **indépendante** de celle de
   la capture : les frames en trop sont écartées avant l'encodeur.

Le DÉCODAGE suit la même règle, et c'est ce qui a remplacé le *pass-through*
du flux MJPEG : une frame n'est décodée que si la détection en a besoin, ou si
la source est déjà compressée. Sur un Raspberry Pi, qui fournit du YUYV, le
chemin nominal ne décode donc rien — le buffer brut part directement à
l'encodeur, qui n'y fait qu'un sous-échantillonnage.

Le Raspberry Pi dispose par ailleurs d'un encodeur matériel (V4L2 M2M,
`/dev/video11`). On a commencé par le logiciel parce qu'il fonctionne à
l'identique sur le poste de développement et sur le Pi — donc qu'il est
testable — et qu'il ne dépend ni d'une version de noyau ni d'un réglage de
`config.txt`.

L'arbitrage, au passage, ne se joue pas qu'en CPU. Les enregistrements en
MJPEG écrivaient ~200 Ko/s sur la carte SD en permanence ; en H.264 c'est
~9 Ko/s, au prix du temps de l'encodeur. Sur un Raspberry Pi, l'usure de la
carte SD n'est pas un détail.

> **Compilation croisée** : contrairement à `ring` et `native-tls` (voir plus
> haut), `openh264` ne pose pas de problème sous QEMU ARM64. Son script de
> compilation ne cherche d'assembleur que pour x86/x86\_64 et construit du C++
> portable pour tout le reste. `OPENH264_NO_ASM=1` le désactive même sur x86,
> si un environnement se montrait récalcitrant.

---

## 📡 Le flux RTSP (`[rtsp]`)

RTSP est le protocole que tous les outils vidéo savent lire : le flux de la
caméra devient consultable depuis un lecteur, enregistrable par un NVR,
intégrable dans une domotique, sans écrire une ligne de code pour chacun.

```bash
vlc   "rtsp://192.168.1.42:8554/stream?token=secret123"
ffplay -rtsp_transport tcp "rtsp://192.168.1.42:8554/stream?token=secret123"
```

Le flux montre **exactement ce que montrent les interfaces web**, boîtes de
détection incrustées comprises — c'est le même flux encodé, publié à un
abonné de plus. Seul le serveur est optionnel : le laisser éteint n'économise
qu'un port ouvert sur le réseau, pas le coût de l'encodeur.

### Authentification

`require_token = true` par défaut : le jeton de `[server] api_token` est
attendu dans la chaîne de requête de l'URL. Ce flux montre la même image que
les interfaces web, qui sont elles authentifiées — l'ouvrir sans contrôle
serait une régression de confidentialité, pas une simplification.

Le jeton est vérifié **une fois par connexion** : un lecteur ne reprend pas
l'URL du `DESCRIBE` pour son `SETUP`, il la reconstruit depuis le SDP, et la
chaîne de requête n'y survit pas. Rien n'est servi avant qu'un jeton valide
n'ait été présenté, et la mémorisation ne vaut que pour la durée de la
connexion TCP.

### Ce qui est géré, et ce qui ne l'est pas

| | |
| --- | --- |
| Transport | RTP dans la connexion TCP (entrelacé) **et** RTP en UDP |
| Profil | H.264 Baseline, `packetization-mode=1` (fragmentation FU-A) |
| RTCP | rapports d'émetteur émis ; les rapports de réception du client sont reconnus et ignorés |
| Non géré | multicast, RTSP sur TLS, authentification Digest, plusieurs pistes (pas d'audio) |

La conformité du flux n'est pas seulement affirmée : un test d'intégration le
fait décoder par **GStreamer**, en TCP et en UDP (`cargo test -p
foxguard-camera --test rtsp_gstreamer`). Il s'ignore de lui-même si
`gst-launch-1.0` n'est pas installé. C'est lui qui a révélé deux
incompatibilités qu'aucune assertion maison n'aurait vues — un lecteur
n'utilise pas la même URL pour son `DESCRIBE`, son `SETUP` et son `PLAY`.

---

## 💾 Les enregistrements

Tout est écrit en `.mp4` — un MP4 fragmenté contenant le flux H.264.

Un format MAISON a précédé : des images JPEG horodatées (`.mjpeg`), chacune
préfixée de son horodatage et de sa longueur. Il avait une qualité, celle de
porter la durée réelle de chaque image, et deux défauts qui l'ont emporté :
une vingtaine de fois plus lourd, et illisible par tout autre logiciel que la
page qui savait le décoder. Le MP4 garde la qualité et perd les défauts.

### Les anciens fichiers

Un `.mjpeg` d'avant la mise à jour peut encore traîner dans `output_record/`.
Il reste **listé, téléchargeable, supprimable et purgé**, mais plus aucun
lecteur ne sait l'ouvrir — les deux interfaces le disent explicitement au lieu
de livrer au navigateur des octets dont il ne fera rien.

L'extension reste donc reconnue par la purge, et ce n'est pas une
inconséquence : l'en retirer ne rendrait pas ces fichiers lisibles, ça les
rendrait **éternels**. Un disque qui se remplit sans jamais se vider est
précisément ce que la rétention existe pour éviter.

### Pourquoi FRAGMENTÉ

Un MP4 ordinaire place sa table des matières à la fin, une fois toutes les
tailles connues. C'est rédhibitoire ici : un enregistrement de
vidéosurveillance peut être interrompu par une coupure de courant, un disque
plein ou un redémarrage — et un MP4 ordinaire amputé de sa fin n'est pas « un
peu abîmé », il est **entièrement illisible**, pas une image ne peut en être
tirée.

Le MP4 fragmenté écrit au contraire un segment d'initialisation complet dès
l'ouverture, puis des fragments autonomes. Un fichier tronqué perd son dernier
fragment, et rien d'autre. Un test le vérifie en coupant volontairement un
fichier en deux et en le faisant décoder.

### Les durées sont réelles

La cadence d'une caméra n'est pas constante — une webcam UVC la réduit en
basse luminosité. Chaque image porte donc sa durée propre, mesurée, et non une
cadence supposée. C'était la seule qualité du format maison ; elle n'est pas
perdue en passant au MP4.

La durée totale, elle, est inscrite à la FERMETURE du fichier. Un fMP4
l'annonce nulle tant qu'il est ouvert ; sans cette correction finale, chaque
lecteur devrait la deviner en parcourant les fragments, et afficherait en
attendant une durée approximative et une barre de progression qui saute.

---

## 💤 Le pré-filtre de mouvement (`[motion]`)

Une caméra de surveillance regarde, l'immense majorité du temps, une scène où
il ne se passe rien. Or l'inférence YOLO est de loin le poste de dépense
dominant du pipeline : la faire tourner sur chacune de ces images identiques,
c'est payer en permanence le prix fort pour réapprendre à chaque passage que
rien n'a changé.

Chaque frame candidate est donc réduite à une grille de 64×48 luminances
**moyennées**, et comparée à la précédente. Au-delà d'une proportion de cases
changées (`min_changed_ratio`), on parle de mouvement — et seulement alors
YOLO tourne.

Le moyennage n'est pas un détail : un simple échantillonnage ponctuel ferait
du bruit de capteur, très présent en basse lumière (précisément quand une
caméra de surveillance sert), un déclencheur permanent, et le filtre ne
filtrerait plus rien.

### Les deux garde-fous

Un filtre naïf rate deux situations, et toutes deux comptent :

1. **quelqu'un s'arrête.** Il ne produit plus de mouvement mais il est
   toujours là. Le filtre garde donc la porte ouverte pendant `hold_secs`
   après le dernier mouvement constaté ;
2. **quelqu'un est déjà immobile.** Comparer deux frames successives est par
   construction aveugle à une présence qui ne bouge pas. YOLO tourne donc de
   toute façon au moins une fois toutes les `max_idle_secs`.

Sans eux, l'économie de CPU se paierait en détections manquées — ce qui n'est
pas un compromis acceptable pour un système de surveillance. C'est aussi
pourquoi le filtre est **actif par défaut** : avec ces garde-fous, il n'a pas
de contrepartie fonctionnelle.

`RUST_LOG=foxguard_camera=debug` journalise chaque décision et la proportion
de l'image qui a changé, ce qui permet de régler les seuils d'après les faits
plutôt qu'au jugé.

---

## 🖥️ Le manager (`crates/manager/`)

Déployé sur le serveur annexe, il s'abonne au broker MQTT sur lequel les
caméras publient et expose leur historique :

| Route | Contenu |
| --- | --- |
| `GET /api/health` | sonde de disponibilité |
| `GET /api/events` | événements récents lus en base (`?limit=`, 100 par défaut, 1000 max) |
| `GET /api/events?date=AAAA-MM-JJ` | **toute** une journée, bornes calculées dans le fuseau du serveur (plafond 5000, signalé par `truncated`) |
| `GET /api/events/{id}/thumbnail` | vignette JPEG d'une détection |
| `GET /api/cameras` | caméras ayant émis au moins un événement encore en mémoire |
| `GET /` | interface React (`[server] ui_dir`) |

Le manager n'a **aucune route d'écriture** : il agrège et expose. Les caméras
renvoyées par `GET /api/cameras` portent l'adresse de leur direct (`live_url`)
et de leur interrupteur de surveillance (`control_url`) — deux pages servies
par la caméra elle-même (voir « Le pilotage depuis le manager »).

Les événements renvoyés portent un identifiant et des URL de média prêtes à
l'emploi (`thumbnail_url`, `clip_url`), toutes deux absentes quand il n'y a
rien à montrer. La liste ne transporte **jamais** les octets des vignettes :
une journée chargée compte des centaines d'événements, et les rapatrier pour
afficher une liste ferait passer des mégaoctets dans une réponse qui n'en a
pas besoin. L'interface demande chaque vignette séparément, au fil du
défilement.

### Persistance

Les événements sont enregistrés dans **PostgreSQL** : l'historique survit au
redémarrage du manager et n'est plus borné par la mémoire du serveur.

Le schéma vit dans `crates/manager/migrations/` et les migrations sont
appliquées **automatiquement au démarrage** — rien à préparer à la main, ni à
la première installation ni après une mise à jour. Une tâche de fond supprime
les événements dépassant `[database] retention_days` (90 jours par défaut,
`0` désactive la purge).

Le manager **refuse de démarrer sans base**, contrairement au broker MQTT dont
l'indisponibilité est tolérée : sans base il perdrait silencieusement tout ce
qu'il reçoit, ce qui est pire qu'un échec franc.

> ⚠️ Un échec d'écriture en base perd l'événement concerné : `rumqttc` a déjà
> acquitté le message au broker, qui ne le renverra pas. Ces cas sont
> journalisés en `ERROR`. Une file de reprise sur disque serait la parade si
> le besoin se confirme.

### La timeline, les clips et le direct

Chaque détection s'accompagne de deux choses, qui ne voyagent **pas** par le
même chemin — et c'est le point de conception à retenir :

| | Où ça vit | Pourquoi |
| --- | --- | --- |
| **La vignette** | dans l'événement MQTT, puis en base | Quelques kilo-octets, publiés à chaque *changement d'état* et non à chaque frame. La timeline doit rester lisible des mois plus tard et depuis n'importe où, alors que la caméra n'est joignable que depuis son réseau local et purge ses fichiers au bout de quelques jours. |
| **Le clip** | sur la caméra, référencé par l'événement | Quelques secondes de vidéo pèsent des mégaoctets, qui n'ont aucune raison de traverser le broker pour un clip que personne n'ouvrira peut-être jamais. |

Conséquence : l'interface du manager, servie par une autre origine, ne peut
pas lire ces fichiers elle-même — le navigateur le lui interdit. Ouvrir les
enregistrements de la caméra à toutes les origines
(`Access-Control-Allow-Origin: *`) serait une bien mauvaise façon de
contourner cette protection, d'autant que ces routes ne sont déjà pas
authentifiées.

**La caméra sert donc elle-même la page qui sait lire son format**
(`GET /play/{fichier}`), et c'est cette page que la timeline affiche dans un
cadre. La politique de même origine est respectée sans rien assouplir, et le
format d'enregistrement reste connu du seul composant qui l'écrit.

**Le direct passe par le même chemin, pour une raison de plus.** La timeline
propose aussi, par caméra, un bouton qui ouvre son flux en direct — et ce flux
est servi par la page `GET /live` de la caméra, affichée dans le même genre de
cadre. Au-delà de la même origine, il y a ici un jeton en jeu : le WebSocket
vidéo de la caméra est authentifié, et c'est la caméra qui injecte son propre
jeton dans la page qu'elle sert. **Le manager ne le reçoit jamais**, ni son
interface, ni sa base. Lui confier le jeton de chaque caméra ferait du serveur
annexe — la pièce la plus exposée du système — le point unique dont la
compromission donne la main sur toutes les caméras.

Pour que ces liens existent, la caméra doit déclarer par quelle URL elle est
joignable depuis un navigateur — elle ne peut pas la deviner :

```toml
[server]
public_url = "http://192.168.1.42:8080"
```

### Le pilotage depuis le manager

La timeline propose aussi, par caméra, un bouton **« 🛡 Surveillance »** qui
active ou coupe sa détection et son enregistrement.

**Le manager ne le fait pourtant pas lui-même**, et c'est le point de
conception : il reste sans aucune route d'écriture. Le pilotage d'une caméra
est authentifié par son jeton d'API, et le lui confier voudrait dire recopier
le jeton de chaque caméra dans sa base de données puis dans une page web —
exactement ce que le direct s'applique à éviter, pour une écriture cette fois.

La caméra sert donc **sa propre page d'interrupteur** (`GET /control`), que le
manager affiche dans le même cadre que le direct et les clips. Elle injecte
elle-même son jeton dans la page, et l'interrupteur appelle
`POST /api/monitoring?token=...` sur la caméra. Le manager ne fait qu'indiquer
l'adresse (`control_url`), au même titre que `live_url`.

Deux détails qui comptent :

* **Cette page n'ouvre pas le WebSocket vidéo**, alors que la commande
  `set_monitoring` y existe déjà. S'abonner au WebSocket est précisément ce qui
  *démarre* l'encodage H.264 : une page réduite à un bouton ferait tourner
  l'encodeur logiciel — le poste de dépense le plus lourd du système — pour une
  image que personne ne regarde. Deux requêtes HTTP ne réveillent personne.
* **L'interface complète de la caméra relit l'état au chargement**
  (`GET /api/monitoring`). Son interrupteur ne connaissait que ses propres
  clics ; maintenant que la surveillance se bascule aussi depuis le manager, il
  afficherait sinon « éteint » sur une caméra qui veille — le pire sens dans
  lequel se tromper.

> ⚠️ Comme `/live`, la page `/control` n'est **pas authentifiée** et porte le
> jeton en clair : qui atteint le port HTTP de la caméra peut la charger, donc
> couper sa surveillance. C'était déjà le modèle de `/live` — le port d'une
> caméra n'est pas destiné à être exposé tel quel sur un réseau hostile — mais
> la conséquence est plus lourde ici, puisqu'il s'agit d'une écriture. La route
> `POST /api/monitoring`, elle, exige bien le jeton.

Laissée vide, ni lecture ni direct ne sont proposés : mieux vaut pas de lien
qu'un lien mort. La vignette, elle, reste visible dans tous les cas.

### Déploiement

Copier `manager-config-sample.toml` en `manager-config.toml` (gitignoré, il
peut contenir des identifiants). `[mqtt] broker_host` et l'URL de base sont
obligatoires — cette dernière peut venir de la variable d'environnement
`DATABASE_URL`, qui prend le pas sur le fichier.

Le `compose.yml` fournit PostgreSQL, et Mosquitto **en option**. Renseignez
d'abord le mot de passe de la base dans un `.env` à côté du compose :

```bash
echo "POSTGRES_PASSWORD=$(openssl rand -base64 24)" > deploy/server/.env
```

Ce fichier est gitignoré : il n'existe pas après un clone, il faut le créer.

**Si vous avez déjà un broker MQTT** (souvent le cas avec Home Assistant ou une
pile Grafana/InfluxDB), renseignez simplement son adresse dans `broker_host` de
`manager-config.toml`, puis :

```bash
docker compose -f deploy/server/compose.yml up -d --build
```

**Si vous n'en avez pas**, ajoutez le profil qui démarre Mosquitto :

```bash
docker compose --profile broker -f deploy/server/compose.yml up -d --build
```

Le broker n'est pas démarré par défaut pour éviter un conflit sur le port 1883
avec celui que vous avez peut-être déjà.

> ⚠️ **`--profile broker` est à répéter sur CHAQUE commande** qui doit voir ce
> conteneur — `down`, `ps`, `logs mosquitto`. Sans lui, Compose fait comme si
> le service n'existait pas, et un `down` laisse le broker tourner.

Selon l'emplacement du broker, `broker_host` prend une valeur différente :

| Le broker tourne… | `broker_host` |
| --- | --- |
| dans ce `compose.yml` (`--profile broker`) | `mosquitto` |
| sur l'hôte du serveur, hors compose | `host.docker.internal` |
| ailleurs sur le réseau | son adresse IP |

Vérification :

```bash
docker compose -f deploy/server/compose.yml logs manager
```

Le journal doit afficher `✅ Connecté au broker MQTT.` puis :

```bash
curl http://localhost:8090/api/events
```

### Déployer sur un serveur distant (archive `.tar`)

Quand le serveur n'a ni le dépôt ni de chaîne de compilation, on lui livre une
image déjà construite — même principe que pour le Raspberry Pi.

**1. Vérifier l'architecture du serveur**, c'est elle qui décide de la
commande de construction :

```bash
ssh mon-serveur uname -m
```

**2. Construire l'image.** Si le serveur a la même architecture que votre
poste (`x86_64` des deux côtés) :

```bash
docker build -f deploy/server/Dockerfile -t foxguard-manager:latest .
```

Si le serveur est en ARM64 (un autre Raspberry Pi, un NAS ARM) :

```bash
docker buildx build --platform linux/arm64 \
  -f deploy/server/Dockerfile \
  -t foxguard-manager:latest \
  --load .
```

**3. Produire l'archive.** Seul le manager est construit par vous ; PostgreSQL
et Mosquitto seront téléchargés par le serveur. S'il n'a pas Internet,
ajoutez-les à l'archive :

```bash
docker save -o foxguard_manager.tar foxguard-manager:latest
```

```bash
docker save -o foxguard_serveur.tar foxguard-manager:latest postgres:16-alpine eclipse-mosquitto:2
```

**4. Copier sur le serveur** l'archive et les quatre fichiers de déploiement :

```bash
scp foxguard_manager.tar deploy/server/compose.deploy.yml deploy/server/mosquitto.conf deploy/server/.env manager-config.toml mon-serveur:~/foxguard/
```

**5. Sur le serveur**, charger l'image et démarrer :

```bash
docker load -i foxguard_manager.tar
```

```bash
docker compose -f compose.deploy.yml up -d
```

Ajoutez `--profile broker` si le serveur n'a pas déjà un broker MQTT.

`compose.deploy.yml` se distingue de `compose.yml` sur deux points : il ne
contient **aucune section `build:`** (le serveur n'a pas les sources) et
n'utilise que des chemins relatifs à lui-même, la configuration étant copiée à
côté de lui plutôt qu'à la racine d'un dépôt.

> ⚠️ `manager-config.toml` doit pointer vers le bon broker : `mosquitto` si
> vous utilisez `--profile broker`, sinon l'adresse du broker existant (voir
> le tableau plus haut).

### Brancher les caméras dessus

Rien n'arrive tant que les caméras ne publient pas : dans le `camera-config.toml` de
CHAQUE Raspberry Pi, activez la section `[mqtt]` et pointez-la vers le broker.

```toml
[camera]
name = "salon"          # distingue les installations dans les événements

[mqtt]
enabled = true          # désactivé par défaut
broker_host = "192.168.1.50"    # ADRESSE IP du serveur, voir ci-dessous
topic = "foxguard/detections"   # doit correspondre au `topic` du manager
```

> ⚠️ Même si le broker est celui du `compose.yml`, les caméras doivent viser
> l'**adresse IP du serveur** et non `mosquitto` : ce nom n'existe que sur le
> réseau interne de Compose, il est introuvable depuis un Raspberry Pi.

Vérification une fois les deux côtés démarrés :

```bash
curl http://<serveur>:8090/api/events
```

### L'interface (`ui/`)

React + TypeScript, construite avec Vite. Elle affiche, **pour une journée
donnée, les détections de chaque caméra connue** — y compris celles sans
aucune détection ce jour-là, car « rien à signaler » et « caméra en panne » se
ressemblent trop sur un écran pour qu'une caméra absente de la liste soit
acceptable.

Chaque caméra présente sa journée de deux façons, parce qu'elles répondent à
deux questions différentes :

* **une bande de 24 heures** répond à *quand s'est-il passé quelque chose ?*
  Une marque par détection, posée à son heure réelle, verte si la personne a
  été reconnue et rouge sinon, plus un repère de l'instant présent sur la
  journée en cours. Une rafale à 3 h du matin et une journée tranquille ne se
  ressemblent pas, et ça se voit avant d'avoir lu une seule ligne ;
* **une pellicule de vignettes** répond à *que s'est-il passé ?* C'est la
  vignette qui fait le travail : un nom et une heure ne disent pas si c'était
  le facteur ou un inconnu dans le jardin.

Les deux sont reliées — cliquer une marque met en évidence la vignette
correspondante, et réciproquement — et un clic sur une vignette ouvre le clip.
Un bouton par caméra ouvre par ailleurs son **flux en direct**, dans le même
cadre, parce que la question qui suit « que s'est-il passé ? » est très
souvent « et maintenant, qu'est-ce qu'on voit ? » (voir « La timeline, les
clips et le direct » plus haut).

Le bundle est produit par une étape Node du `Dockerfile` du manager : l'image
ne dépend d'aucun `npm run build` lancé à la main.

En développement, avec le manager déjà démarré :

```bash
cd ui && npm install && npm run dev
```

Vite sert alors l'interface sur son propre port en relayant `/api` vers
`http://localhost:8090` (réglable par `FOXGUARD_API`), ce qui reproduit
l'origine unique de la production : le code d'appel est identique dans les
deux cas, sans CORS ni URL d'API à injecter.

Les types de l'API sont **générés** depuis les types Rust par `ts-rs`
(`ui/src/generated/`, régénérés par `cargo test --workspace` et commités).
Renommer un champ côté Rust casse donc la compilation de l'interface. Voir
`ui/README.md` pour le détail et l'étape de CI qui détecte un fichier généré
obsolète.

### Tests

Les tests du dépôt PostgreSQL demandent une vraie base : ils vérifient les
migrations, le SQL et la conversion des horodatages, ce qui n'a aucun sens
contre une imitation. Ils **s'ignorent d'eux-mêmes** si
`FOXGUARD_TEST_DATABASE_URL` n'est pas renseignée, pour qu'un `cargo test` sur
un poste sans PostgreSQL reste vert — pensez donc à la renseigner en CI, sans
quoi ils ne vérifient rien.

```bash
docker run -d --rm --name fg-pg -e POSTGRES_PASSWORD=secret \
    -e POSTGRES_USER=foxguard -e POSTGRES_DB=foxguard \
    -p 55432:5432 postgres:16-alpine

FOXGUARD_TEST_DATABASE_URL=postgres://foxguard:secret@localhost:55432/foxguard \
    cargo test -p foxguard-manager --test postgres
```

Chaque test travaille dans son propre schéma PostgreSQL : ils peuvent tourner
en parallèle sans se marcher dessus.

Les tests de l'API HTTP (`tests/api_postgres.rs`) demandent la même base et
s'ignorent de la même façon. Ils vérifient ce que l'interface reçoit
réellement : la forme du JSON, les URL de média, les en-têtes de la vignette.

> **État actuel** : la chaîne caméra → MQTT → manager → PostgreSQL → HTTP →
> interface marche de bout en bout, avec timeline, vignettes et accès aux
> clips. L'API reste en **lecture seule** — le pilotage des caméras passe par
> leur interface embarquée. Les notifications restent à construire.

---

## 📜 Journalisation

L'application utilise `tracing`. Le niveau se règle par la variable
d'environnement **`RUST_LOG`** :

```bash
RUST_LOG=foxguard_camera=debug cargo run -p foxguard-camera
```

Par défaut (`foxguard_camera=info,warn`), seuls les événements de cycle de vie
apparaissent : démarrage, visages de référence chargés, clients WebSocket,
enregistrements, erreurs.

Le niveau `debug` ajoute les diagnostics par visage détecté et par personne
suivie — notamment la similarité obtenue face à chaque gabarit, qui est le
moyen le plus direct de régler `face_match_threshold`. Ces messages sont
volontairement muets par défaut : ils sont émis depuis les tâches parallèles
du pipeline de vision, à raison de plusieurs par seconde.

Le manager a son propre filtre (`RUST_LOG=foxguard_manager=debug`).

---

## ⚙️ Configuration (`camera-config.toml`)

Les fichiers de configuration vivent à la **racine du dépôt**, un par
composant : `camera-config.toml` et `manager-config.toml` (modèles :
`camera-config-sample.toml` et `manager-config-sample.toml`, les deux fichiers
réels étant gitignorés car ils contiennent des identifiants).

C'est la racine et non les crates, parce que `cargo run -p <composant>` exécute
le binaire avec le répertoire de travail positionné sur la **racine du
workspace** : un fichier placé dans `crates/camera/` y serait introuvable. Un
fichier de configuration est par ailleurs une donnée de déploiement, montée ou
copiée à côté du binaire, et non du code qui voyage dans le crate.

Partez de `camera-config-sample.toml` pour créer votre propre `camera-config.toml`.

* **`[server]`** : `host`, `port`, `api_token` (jeton exigé en paramètre `?token=` pour se connecter au WebSocket et, si `[rtsp] require_token`, au flux RTSP), `public_url` (URL par laquelle cette caméra est joignable depuis un navigateur, ex. `"http://192.168.1.42:8080"` — publiée dans les événements pour que la timeline du manager offre un lien vers les clips **et vers le direct de la caméra** ; vide par défaut, auquel cas aucun lien n'est proposé).
* **`[camera]`** : `device_index` (index du périphérique V4L2, ex. `0` pour `/dev/video0`), `name` (nom de la caméra inclus dans les événements MQTT, optionnel — `"foxguard"` par défaut).
* **`[detection]`** : `enabled` (surveillance active au démarrage), chemins des 3 modèles ONNX (`model_path`, `model_detect_face_path`, `model_face_path`, tous dans `crates/camera/models/`), tailles d'entrée (`input_size` pour YOLO, `input_face_size` pour ArcFace), `confidence_threshold` (seuil de détection YOLO), `email_cooldown_secs` et `known_faces_dir` (dossier des photos de référence, `"known_faces"` par défaut, relatif au répertoire de travail — à renseigner en absolu pour un déploiement en conteneur ou en service systemd).
* **`[email]`** : `enabled`, identifiants SMTP (`smtp_server`, `smtp_user`, `smtp_password`), `from_address`, `to_address`.
* **`[recording]`** *(optionnel, section entière absente = valeurs par défaut)* : `dir` (dossier des enregistrements, `"output_record"` par défaut, relatif au répertoire de travail — à renseigner en absolu pour un déploiement en conteneur ou en service systemd), `retention_days` (durée de conservation des enregistrements, en jours — **`0` désactive entièrement la suppression automatique**) et `cleanup_interval_secs` (intervalle entre deux passages, `3600` par défaut). Un passage a aussi lieu au démarrage, pour purger ce qui a expiré pendant un arrêt prolongé. L'âge est déterminé par la date de dernière modification du fichier, jamais par son nom : un enregistrement en cours d'écriture ne peut donc pas être supprimé sous la caméra. La même section règle les **clips d'événement** : `clips_enabled`, `clip_pre_secs` (durée conservée *avant* la détection, 4 s par défaut) et `clip_post_secs` (8 s). Les clips partagent le dossier, le format et la rétention des enregistrements — ils sont donc listés, servis, supprimés et purgés par le même code.
* **`[mqtt]`** *(optionnel, section entière absente = désactivé)* : `enabled`, `broker_host`, `broker_port` (`1883` par défaut), `username`/`password` (authentification optionnelle, pas de TLS), `topic` (`"foxguard/detections"` par défaut). Publie un message JSON à chaque changement d'état de reconnaissance, par exemple :
  ```json
  {"camera": "salon", "timestamp": "2026-09-18T15:42:07+02:00", "status": "known", "name": "jerome",
   "thumbnail": "<JPEG en base64>", "base_url": "http://192.168.1.42:8080", "clip": "evt_20260918_154207123.mp4"}
  {"camera": "salon", "timestamp": "2026-09-18T15:45:12+02:00", "status": "unknown", "thumbnail": null, "base_url": null, "clip": null}
  ```
* **`[motion]`** *(optionnel, section entière absente = **actif** avec ses valeurs par défaut)* : `enabled`, `pixel_threshold` (écart de luminance à partir duquel un pixel a changé, `20`), `min_changed_ratio` (proportion de l'image qui doit avoir changé, `0.006`), `hold_secs` (durée pendant laquelle YOLO continue après le dernier mouvement, `3`) et `max_idle_secs` (délai maximal entre deux passages même sans mouvement, `20` — **`0` désactive ce passage périodique**). Voir « Le pré-filtre de mouvement » plus haut pour le rôle des deux derniers.
* **`[h264]`** *(optionnel, section entière absente = ses valeurs par défaut)* : `fps` (`12`), `bitrate_kbps` (`1500`) et `keyframe_interval_secs` (`2`). **Pas d'interrupteur** : le H.264 est le seul chemin vidéo de la caméra, et le désactiver ne laisserait rien à diffuser ni à enregistrer. Ces réglages pilotent l'encodeur, dont le flux alimente **trois** consommateurs : le WebSocket des interfaces web, les enregistrements et le RTSP. Ils vivent donc ici et non dans `[rtsp]`, qui n'en est qu'un des trois. Voir « L'encodage H.264 » plus haut.
* **`[rtsp]`** *(optionnel, section entière absente = serveur désactivé)* : `enabled`, `host`, `port` (`8554`), `path` (`"stream"`) et `require_token` (`true`) — uniquement les réglages RÉSEAU du serveur RTSP. Le laisser éteint n'économise qu'un port ouvert, pas le coût de l'encodage. Voir « Le flux RTSP » plus haut.

---

## 🛠️ Compilation et Lancement

Le dépôt étant un workspace, chaque composant se lance avec `-p`. En mode
optimisé (recommandé pour les performances de l'inférence IA) :

```bash
cargo run --release -p foxguard-camera
```

⚠️ **Depuis la racine du dépôt** : les chemins de `camera-config.toml` (modèles ONNX,
dossiers de données) sont relatifs au répertoire de travail.

Le manager, lui, se lance sur le serveur annexe — pas sur le Raspberry Pi :

```bash
cargo run --release -p foxguard-manager
```

Il lit `manager-config.toml` (modèle : `manager-config-sample.toml`).

---

## 🚀 Docker — image de la caméra (Raspberry Pi)

Cette section ne concerne que `foxguard-camera`. Pour le manager, qui tourne
sur le serveur annexe, voir la section « Le manager » plus haut.

### Construire l'image Docker 

```bash
docker build -f deploy/camera/Dockerfile -t foxguard-camera:latest .
```

Vérifiez que l'image contient bien le vrai binaire — quelques mégaoctets, pas
quelques centaines de kilooctets :

```bash
docker run --rm --entrypoint sh foxguard-camera:latest -c 'ls -lh /app/foxguard-camera'
```

Un binaire de ~370 Ko signalerait que l'étape de mise en cache des dépendances
a livré son programme factice à la place du vrai : le conteneur se terminerait
alors immédiatement, code de sortie 0, **sans le moindre message**. Le
`Dockerfile` s'en prémunit (voir le `touch` après la copie des sources), mais
c'est un mode de panne assez déroutant pour mériter une vérification.

### Lancer le conteneur avec accès à la caméra locale (/dev/video0)

Comme l'application interagit avec la caméra webcam V4L2 locale, il faut lui passer le périphérique vidéo (--device) et persister le dossier des enregistrements (-v)

```bash
docker run -d \
  --name foxguard_app \
  --device=/dev/video0:/dev/video0 \
  -p 8080:8080 \
  -v $(pwd)/output_record:/app/output_record \
  -v $(pwd)/known_faces:/app/known_faces \
  foxguard-camera:latest
```

Ajoutez `-p 8554:8554` si vous avez activé le flux RTSP (`[rtsp] enabled =
true`) : le port est déclaré dans l'image, mais il faut encore le publier.

`known_faces/` est monté lui aussi : sans cela, les photos de référence
capturées depuis l'interface web vivent dans le système de fichiers du
conteneur et disparaissent à sa recréation.

> **⚠️ Droits des dossiers montés.** Le conteneur tourne sous un utilisateur
> non-root d'UID **10001** (voir `deploy/camera/Dockerfile`), alors qu'un
> dossier créé sur l'hôte appartient à votre utilisateur. Sans ajustement, le
> conteneur ne peut rien y écrire et l'activation de la surveillance échoue
> avec `Permission denied (os error 13)`. Préparez les dossiers une fois pour
> toutes avant le premier lancement :
>
> ```bash
> mkdir -p output_record known_faces
> sudo chown -R 10001:10001 output_record known_faces
> ```

### Construire l'image pour le raspberry

Utiliser Docker Buildx pour cibler l'architecture ARM64 (linux/arm64).

1. Activer l'émulation multi-architecture sur votre PC

```bash
docker run --privileged --rm tonistiigi/binfmt --install all
```

2. Créer un builder Docker spécifique

```bash
docker buildx create --name rpi-builder --use
docker buildx inspect --bootstrap
```

3. Builder et exporter l'image pour ARM64

Option 1 : Sans utilisation de Docker Hub ou GitHub Container Registry

1. Buildez et sauvegardez l'image dans un fichier .tar

```bash
docker buildx build --platform linux/arm64 \
  -f deploy/camera/Dockerfile \
  -t foxguard-camera:rpi4 \
  --output type=docker,dest=foxguard_rpi4.tar .
```

1. Copiez le fichier sur le Raspberry Pi :

```bash
scp foxguard_rpi4.tar foxguard@foxguard.local:/home/foxguard/
```

3. Chargez l'image sur le Raspberry Pi

```bash
# Sur le Raspberry Pi :
docker load -i foxguard_rpi4.tar
```

Option 2 : Publier sur Docker Hub

```bash
docker buildx build --platform linux/arm64 \
  -f deploy/camera/Dockerfile \
  -t jprecigout/foxguard-camera:rpi4 \
  --push .
```

sur le Raspberry Pi

```bash
docker pull jprecigout/foxguard-camera:rpi4
```

## 🛠️ Modification de la configuration du rapsberry pour activer le pilote V4L2 Legacy
Il faut configurer le Raspberry Pi pour qu'il utilise le contrôleur vidéo hérité compatible V4L2 natif. (Necessaire pour les cameras branchées avec une nappe CSI)

1. Modifier la configuration du Raspberry Pi (sur l'hôte)
   
Ouvrez le fichier de configuration de démarrage du Pi :

```bash
# Sur Raspberry Pi OS Bookworm :
sudo nano /boot/firmware/config.txt

# For more options and information see
# http://rptl.io/configtxt
# Some settings may impact device functionality. See link above for details

# Uncomment some or all of these to enable the optional hardware interfaces
#dtparam=i2c_arm=on
#dtparam=i2s=on
#dtparam=spi=on

# Enable audio (loads snd_bcm2835)
dtparam=audio=on

# Additional overlays and parameters are documented
# /boot/firmware/overlays/README

# Désactivation du pilote moderne libcamera / Unicam
# camera_auto_detect=1
camera_auto_detect=0

# Activation du pilote V4L2 hérité pour la caméra CSI
start_x=1
gpu_mem=128

# Automatically load overlays for detected DSI displays
display_auto_detect=1

# Automatically load initramfs files, if found
auto_initramfs=1

# Enable DRM VC4 V3D driver
dtoverlay=vc4-kms-v3d
max_framebuffers=2

# Don't have the firmware create an initial video= setting in cmdline.txt.
# Use the kernel's default instead.
disable_fw_kms_setup=1

# Run in 64-bit mode
arm_64bit=1

# Disable compensation for displays with overscan
disable_overscan=1

# Run as fast as firmware / board allows
arm_boost=1

[cm4]
# Enable host mode on the 2711 built-in XHCI USB controller.
# This line should be removed if the legacy DWC2 controller is required
# (e.g. for USB device mode) or if USB support is not required.
otg_mode=1

[cm5]
dtoverlay=dwc2,dr_mode=host

[all]
```
2. Charger le module et redémarrer
   
Exécutez ces commandes puis redémarrez le Pi :

```bash
echo "bcm2835-v4l2" | sudo tee /etc/modules-load.d/bcm2835-v4l2.conf
sudo reboot
```

### 📦 Lancer le conteneur sur le Raspberry Pi 4

Une fois l'image disponible sur le Raspberry Pi (via docker build local, docker load ou docker pull), exécutez le conteneur en transmettant le périphérique caméra /dev/video0

```bash
docker run -d \
  --name foxguard \
  --restart=always \
  --privileged \
  -v /dev:/dev \
  -p 8080:8080 \
  -p 8554:8554 \
  -v $(pwd)/output_record:/app/output_record \
  -v $(pwd)/known_faces:/app/known_faces \
  foxguard-camera:rpi4
```

Mêmes remarques que ci-dessus sur le montage de `known_faces/` et sur les
droits des dossiers (`sudo chown -R 10001:10001 output_record known_faces`).
Le port `8554` est celui du flux RTSP : sans effet tant que `[rtsp] enabled`
reste à `false`.
---

## 🌐 Utilisation de l'Interface Web

Chaque caméra embarque **sa propre** interface, servie directement par le
Raspberry Pi. Elle reste le poste de pilotage d'une caméra donnée — et le
secours qui fonctionne encore quand le serveur annexe est en panne ou
injoignable. L'interface d'ENSEMBLE (plusieurs caméras, timeline des
détections avec vignettes, clips et accès au direct) est celle du manager
(voir `ui/`).

Ouvrez votre navigateur web et rendez-vous sur l'adresse de la caméra (par exemple : http://localhost:8080 ou http://foxguard.local:8080).

Le flux vidéo s'établit automatiquement via WebSocket, en **H.264** décodé par
le navigateur lui-même (WebCodecs). C'est le seul format diffusé : un
navigateur qui ne sait pas décoder le H.264 (Chrome/Edge < 94, Safari < 16.4,
Firefox < 130) l'affiche en clair à la place du flux, plutôt que de laisser un
cadre noir. Le pilotage et les enregistrements, eux, restent accessibles.

Utilisez l'interface pour :

* Activer ou désactiver la surveillance (détection IA + enregistrement) — aussi faisable depuis l'interface du manager, voir « Le pilotage depuis le manager ».
* Capturer une photo de référence webcam pour la reconnaissance faciale (carte « 📸 Photo de référence »), pour un nom donné ; chaque capture s'ajoute aux précédentes pour ce nom.
* Consulter, actualiser, relire et supprimer les enregistrements vidéo sauvegardés.

---

## Préparation du raspberry

### Installation de docker sur le raspberry

```bash
# Installation de docker
curl -fsSL https://get.docker.com -o get-docker.sh
sudo sh get-docker.sh

# Nettoyage 
rm get-docker.sh

# Eviter l'execution par root : Ajout de l'utilisateur actuel au groupe docker
sudo usermod -aG docker $USER

# Appliquez les changements de groupe immédiatement
newgrp docker

# Activer le démarrage automatique
sudo systemctl enable docker
sudo systemctl start docker
```

### Installation de portainer

```bash
docker volume create portainer_data
docker run -d \
  -p 9000:9000 \
  --name portainer \
  --restart=always \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v portainer_data:/data \
  portainer/portainer-ce:latest
```

Accès via l'interface web : http://foxguard.local:9000
