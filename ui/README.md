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
| `GET /`           | ce bundle (`[server] ui_dir` de `manager-config.toml`) |
| `GET /api/health` | sonde de disponibilité                          |
| `GET /api/events` | événements récents (`?limit=`)                  |
| `GET /api/cameras`| caméras ayant émis au moins un événement        |

Un seul conteneur, une seule origine : **pas de CORS à configurer**, pas de
nginx supplémentaire.

## Le contrat de types

Les types de l'API ne sont **pas écrits à la main** : ils sont générés depuis
les types Rust par [`ts-rs`](https://github.com/Aleph-Alpha/ts-rs) et déposés
dans `src/generated/`.

```bash
cargo test -p foxguard-manager        # régénère src/generated/
```

Les fichiers générés sont **commités** : l'interface se construit sans chaîne
Rust, et une revue voit passer les changements de contrat. Si vous modifiez un
type d'API côté Rust sans relancer la génération, le fichier commité devient
obsolète — une étape de CI le détecte :

```bash
cargo test -p foxguard-manager && git diff --exit-code ui/src/generated/
```

`ts-rs` est derrière la feature `ts` de `foxguard-protocol`, que seul
`foxguard-manager` active : la caméra ne l'embarque pas dans sa compilation
croisée ARM64.

### Ce que ça garantit

Renommer ou supprimer un champ côté Rust **casse la compilation** de
l'interface, au lieu de produire une valeur `undefined` à l'exécution.

Le gain est aussi qualitatif. `DetectionEvent` est généré en union
discriminée, fidèle au `#[serde(flatten)]` du Rust :

```ts
type DetectionEvent = { camera: string; timestamp: string } &
  ({ status: "unknown" } | { status: "known"; name: string });
```

TypeScript refuse donc `event.name` sans avoir d'abord vérifié
`event.status === "known"` — l'état incohérent « inconnu avec un nom » n'est
pas représentable, ce qu'une définition écrite à la main (`name?: string`)
autorisait.

⚠️ Ce que ça ne garantit **pas** : la cohérence entre versions DÉPLOYÉES.
L'interface et le manager étant servis par le même conteneur, ils avancent
ensemble — mais c'est une propriété du déploiement, pas du typage.
