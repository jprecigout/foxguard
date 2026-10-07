# foxguard-ui

Interface web du **manager** (React), déployée sur le serveur annexe.

Elle affiche, pour une journée donnée, la **timeline des détections de chaque
caméra** : une bande de 24 heures qui situe les événements, une pellicule de
vignettes qui montre ce qui s'est passé, un accès direct au clip de chaque
détection, et un bouton par caméra vers son **flux en direct**.

Un second onglet, **« ● Direct »**, affiche la **mosaïque des directs** de
toutes les caméras, ou une seule en grand.

## Ce que ce n'est pas

Ce n'est **pas** l'interface de la caméra. Chaque caméra embarque déjà la
sienne (`crates/camera/static/controller.html`), servie directement par le
Raspberry Pi : flux vidéo en direct, activation de la surveillance, capture de
photos de référence, relecture des enregistrements.

Cette interface embarquée doit être **conservée** même une fois `foxguard-ui`
en place : c'est le secours qui reste disponible quand le serveur est en panne
ou injoignable depuis le réseau du Pi. Sur un système de surveillance, ça
compte.

`foxguard-ui` est la vue d'ENSEMBLE : plusieurs caméras, historique agrégé des
détections, recherche, notifications.

## Identité visuelle

`public/logo.svg` est une **copie** de `crates/camera/assets/logo.svg`, et la
palette de `src/styles.css` reprend celle de l'interface embarquée de la
caméra (`crates/camera/static/controller.html`) : les deux interfaces sont le
même produit et ne doivent pas avoir deux identités.

La copie est délibérée. L'étape Node du Dockerfile ne reçoit que le dossier
`ui/` comme contexte, et `npm run dev` travaille depuis ce même dossier :
référencer un fichier situé hors de `ui/` casserait l'un ou l'autre. Pour un
SVG de 3 Ko, la duplication coûte moins cher que le couplage — mais si le logo
évolue, **les deux copies sont à mettre à jour**.

## Organisation du code

| Fichier | Rôle |
| --- | --- |
| `src/App.tsx` | composition : onglets, vue courante, ouverture du cadre |
| `src/route.ts` | navigation par fragment d'URL (`#/`, `#/direct`, `#/direct/<caméra>`) |
| `src/components/TimelineView.tsx` | la vue timeline : chargement d'une journée, état partagé entre la bande et la pellicule |
| `src/components/LiveWall.tsx` | la mosaïque des directs |
| `src/components/LiveStream.tsx` | le direct d'une caméra : ticket, WebSocket, décodage WebCodecs vers un canevas |
| `src/frames.ts` | les pages de caméra ouvertes dans le cadre (clip, interrupteur) |
| `src/components/DayBar.tsx` | navigation entre les journées |
| `src/components/CameraTimeline.tsx` | la timeline d'une caméra : bande de 24 h et pellicule de vignettes |
| `src/components/CameraFrameDialog.tsx` | cadre affichant une page servie par la CAMÉRA : le clip d'une détection, ou son interrupteur |
| `src/timeline.ts` | géométrie temporelle de la bande (position d'un horodatage, graduations, repère « maintenant ») |
| `src/grouping.ts` | regroupement des événements par caméra |
| `src/dates.ts` | manipulation des journées, en heure locale |
| `src/api.ts` | appels de l'API du manager |
| `src/generated/` | types d'API générés depuis le Rust (voir plus bas) |

La logique vit dans `timeline.ts`, `grouping.ts` et `dates.ts` plutôt que dans
les composants : ce sont des fonctions pures, et ce sont les seules choses de
l'interface qui méritent d'être relues attentivement.

## Les clips, le direct et l'interrupteur

Cette interface donne accès à deux pages qu'elle ne produit **pas
elle-même** : le clip d'une détection et l'interrupteur de surveillance d'une
caméra. Les deux appartiennent à la caméra, donc à une autre origine, et c'est
la caméra qui sert la page capable de les afficher. `CameraFrameDialog` ne
fait que mettre cette page dans un cadre :

| Média | Page servie par la caméra | D'où vient l'URL |
| --- | --- | --- |
| Le clip d'une détection | `GET /play/<fichier>` | `clip_url` de l'`EventRecord` |
| L'interrupteur de surveillance | `GET /control` | `control_url` du `CameraInfo` |

Le **direct**, lui, est décodé ici même (`LiveStream.tsx`) : l'interface
demande au manager un ticket de visionnage (`stream_url` du `CameraInfo`),
puis ouvre avec lui le WebSocket de la caméra. Voir « La mosaïque des
directs » ci-dessous.

Ouvrir les enregistrements de la caméra à toutes les origines serait une bien
mauvaise façon de contourner la politique de même origine, d'autant que ces
routes ne sont déjà pas authentifiées.

**L'interrupteur suit exactement le même chemin, et c'est ce qui permet au
manager de rester en lecture seule.** Le bouton « 🛡 Surveillance » n'appelle
aucune route du manager : il ouvre la page `/control` de la caméra, qui porte
le jeton injecté par la caméra et appelle `POST /api/monitoring?token=...` sur
elle-même. Le manager se contente d'indiquer l'adresse (`control_url`). Lui
confier le pilotage voudrait dire recopier le jeton de chaque caméra dans sa
base, et faire du serveur annexe le point unique dont la compromission donne
la main sur toutes les caméras — voir « Le pilotage depuis le manager » du
README principal.

Conséquence assumée : clip, direct et interrupteur demandent que la **caméra**
soit joignable depuis le navigateur, et aucun des trois n'est proposé si elle
n'a pas déclaré son URL publique (`clip_url`, `stream_url` et `control_url`
valent `null` sinon). La vignette, elle, vient de la base du manager et reste
visible dans tous les cas.

## La mosaïque des directs

Le WebSocket vidéo d'une caméra est authentifié par son jeton d'API, qui donne
tous les droits sur elle : ni cette interface, ni le manager ne le
connaissent. Le manager partage en revanche avec les caméras un secret qui lui
permet de signer des **tickets de visionnage** : liés à une caméra, valables
deux minutes, et qui n'ouvrent le flux qu'en **lecture seule**.

`LiveStream` enchaîne donc, à chaque (re)connexion :

1. `GET <stream_url>` sur le manager → `{ "url": "ws://<caméra>/ws?ticket=…" }` ;
2. ouverture de ce WebSocket, directement auprès de la caméra ;
3. décodage avec WebCodecs (`VideoDecoder`) vers un canevas — le protocole de
   `crates/camera/static/live.html`, avec lequel il doit rester aligné.

Chaque flux ouvert fait tourner l'encodeur H.264 de sa caméra : le composant
ferme la connexion quand l'onglet est masqué, et la mosaïque ferme toutes les
siennes quand on la quitte.

`stream_url` vaut `null` — et la vignette le dit — si la caméra n'a pas
déclaré son URL publique, ou si le manager n'a pas de `[stream]
ticket_secret`. Configuration : voir « La mosaïque des directs » du README
principal.

## Toolchain

Hors du workspace Cargo : npm/pnpm et son propre lockfile. Les deux
n'interfèrent pas, mais `node_modules/` et `dist/` doivent rester hors de git
et hors des contextes de build Docker (voir `.gitignore` et `.dockerignore`).

## Comment elle est servie

Le manager sert le bundle statique à la racine, et son API sous `/api/*`
(voir `crates/manager/src/api.rs`) :

| Route             | Contenu                                        |
| ----------------- | ---------------------------------------------- |
| `GET /`           | ce bundle (`[server] ui_dir` de `manager-config.toml`) |
| `GET /api/health` | sonde de disponibilité                          |
| `GET /api/events` | événements récents (`?limit=`) ou d'une journée (`?date=`) |
| `GET /api/events/{id}/thumbnail` | vignette JPEG d'une détection   |
| `GET /api/cameras`| caméras ayant émis au moins un événement, avec l'URL de leur direct |
| `GET /api/cameras/{nom}/stream` | ticket de visionnage du direct d'une caméra (URL WebSocket, à usage immédiat) |

Un seul conteneur, une seule origine : **pas de CORS à configurer**, pas de
nginx supplémentaire.

## Le contrat de types

Les types de l'API ne sont **pas écrits à la main** : ils sont générés depuis
les types Rust par [`ts-rs`](https://github.com/Aleph-Alpha/ts-rs) et déposés
dans `src/generated/`.

```bash
cargo test --workspace                # régénère src/generated/
```

`--workspace` et non `-p foxguard-manager` : `ts-rs` installe un test par type
dérivant `TS`, **dans le crate où ce type est défini**. `EventRecord`,
`EventsResponse` et `CameraInfo` vivent dans le manager, mais
`DetectionEvent` et `PersonStatus` vivent dans `foxguard-protocol` — et leurs
tests ne sont compilés que si la feature `ts` y est active, ce que
l'unification des features du workspace assure.

Les fichiers générés sont **commités** : l'interface se construit sans chaîne
Rust, et une revue voit passer les changements de contrat. Si vous modifiez un
type d'API côté Rust sans relancer la génération, le fichier commité devient
obsolète — une étape de CI le détecte :

```bash
cargo test --workspace && git diff --exit-code ui/src/generated/
```

`ts-rs` est derrière la feature `ts` de `foxguard-protocol`, que seul
`foxguard-manager` active : la caméra ne l'embarque pas dans sa compilation
croisée ARM64.

### Ce que ça garantit

Renommer ou supprimer un champ côté Rust **casse la compilation** de
l'interface, au lieu de produire une valeur `undefined` à l'exécution.

Le gain est aussi qualitatif. `EventRecord` — le type que consomme cette
interface — est généré en union discriminée, fidèle au `#[serde(flatten)]` du
Rust :

```ts
type EventRecord = {
  id: number;
  camera: string;
  timestamp: string;
  thumbnail_url: string | null;
  clip_url: string | null;
} & ({ status: "unknown" } | { status: "known"; name: string });
```

TypeScript refuse donc `event.name` sans avoir d'abord vérifié
`event.status === "known"` — l'état incohérent « inconnu avec un nom » n'est
pas représentable, ce qu'une définition écrite à la main (`name?: string`)
autorisait.

> `EventRecord` (le format de l'**API HTTP**) est distinct de `DetectionEvent`
> (le format du **fil MQTT**), et c'est délibéré : l'interface a besoin d'un
> identifiant et d'URL de média prêtes à l'emploi, pas de la vignette encodée
> en base64 qui alourdirait la réponse d'une journée de plusieurs mégaoctets.

⚠️ Ce que ça ne garantit **pas** : la cohérence entre versions DÉPLOYÉES.
L'interface et le manager étant servis par le même conteneur, ils avancent
ensemble — mais c'est une propriété du déploiement, pas du typage.
