# foxguard-ui

Interface web du **manager** (React), déployée sur le serveur annexe.

> Non encore implémentée. Ce dossier réserve l'emplacement et documente
> comment le composant s'insère dans le reste du projet.

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

## Toolchain

Hors du workspace Cargo : npm/pnpm et son propre lockfile. Les deux
n'interfèrent pas, mais `node_modules/` et `dist/` doivent rester hors de git
et hors des contextes de build Docker (voir `.gitignore` et `.dockerignore`).

## Comment elle est servie

Le manager sert le bundle statique à la racine, et son API sous `/api/*`
(voir `crates/manager/src/api.rs`) :

| Route             | Contenu                                        |
| ----------------- | ---------------------------------------------- |
| `GET /`           | ce bundle (`[server] ui_dir` de `manager.toml`) |
| `GET /api/health` | sonde de disponibilité                          |
| `GET /api/events` | événements récents (`?limit=`)                  |
| `GET /api/cameras`| caméras ayant émis au moins un événement        |

Un seul conteneur, une seule origine : **pas de CORS à configurer**, pas de
nginx supplémentaire.

## Le contrat de types

C'est le point à ne pas rater. Le crate `foxguard-protocol` garantit à la
compilation que la caméra et le manager parlent le même langage — mais
TypeScript ne sait rien des types Rust, et cette frontière-là est donc à
nouveau exposée à la dérive silencieuse.

La réponse la plus légère est [`ts-rs`](https://github.com/Aleph-Alpha/ts-rs) :
une macro `derive` sur les types d'API du manager, qui génère les `.d.ts`
pendant `cargo test`. Les types générés sont commités, et un changement côté
Rust casse alors la compilation TypeScript.

⚠️ Ne réutilisez pas `DetectionEvent` tel quel comme type d'API : c'est le
format de FIL MQTT entre caméra et manager. L'interface voudra des vues
agrégées (dernière détection par caméra, historique paginé, miniatures). Deux
types distincts, sinon le format de fil se retrouve contraint par les besoins
d'affichage et ne peut plus évoluer sans casser les Raspberry Pi déjà
déployés.
