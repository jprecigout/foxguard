# Image du MANAGER FoxGuard (crates/manager), déployée sur le serveur annexe.
#
# À construire depuis la RACINE du workspace :
#   docker build -f deploy/server/Dockerfile -t foxguard-manager:latest .
#
# Contrairement à l'image de la caméra, celle-ci se construit pour
# l'architecture du serveur (x86_64 en général) : pas d'émulation QEMU, pas de
# contrainte sur les dépendances contenant de l'assembleur.

# ==========================================
# 1. STAGE DE COMPILATION (BUILD)
# ==========================================
FROM rust:1-slim AS builder

WORKDIR /app

# Mise en cache des dépendances Cargo (voir la note détaillée dans
# deploy/camera/Dockerfile : Cargo a besoin de tous les manifestes du
# workspace, même pour ne construire qu'un membre).
COPY Cargo.toml Cargo.lock ./
COPY crates/camera/Cargo.toml crates/camera/
COPY crates/manager/Cargo.toml crates/manager/
COPY crates/protocol/Cargo.toml crates/protocol/
RUN mkdir -p crates/camera/src crates/manager/src crates/protocol/src \
    && echo "fn main() {}" > crates/camera/src/main.rs \
    && : > crates/camera/src/lib.rs \
    && echo "fn main() {}" > crates/manager/src/main.rs \
    && : > crates/manager/src/lib.rs \
    && : > crates/protocol/src/lib.rs

# `-p foxguard-manager` : les dépendances lourdes de la caméra (V4L2, ONNX,
# traitement d'image) ne sont JAMAIS compilées ici.
RUN cargo build --release --locked -p foxguard-manager
RUN rm -rf crates/camera/src crates/manager/src crates/protocol/src

# Copie du code source réel.
#
# `touch` INDISPENSABLE : `COPY` préserve les dates de modification du
# contexte de build, donc les sources arrivent plus ANCIENNES que les
# artefacts du build de cache ci-dessus. Cargo les considérerait à jour et ne
# recompilerait rien — l'image embarquerait alors le binaire factice
# `fn main() {}`, qui se termine immédiatement sans le moindre message.
COPY crates ./crates
RUN find crates -name '*.rs' -exec touch {} +

# Compilation finale du binaire
RUN cargo build --release -p foxguard-manager

# ==========================================
# 2. STAGE D'EXÉCUTION (RUNTIME SÉCURISÉ)
# ==========================================
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    tzdata \
    && rm -rf /var/lib/apt/lists/*

ENV TZ=Europe/Paris

# 🔒 Utilisateur non-root. Le manager ne touche à aucun périphérique : il n'a
# pas besoin du groupe `video`, contrairement à la caméra.
RUN groupadd -g 10001 foxguard && \
    useradd -u 10001 -g foxguard -m -s /bin/false foxguard

WORKDIR /app

RUN mkdir -p /app/ui/dist && chown -R foxguard:foxguard /app

COPY --from=builder --chown=foxguard:foxguard /app/target/release/foxguard-manager /app/foxguard-manager

# Le dossier de l'interface est créé VIDE ci-dessus : le manager se contente
# alors de servir son API, `/` retournant 404. Quand `ui/` existera, c'est une
# étape de build Node qui produira le bundle et le copiera ici — pas un `COPY`
# depuis l'hôte, qui dépendrait d'un `npm run build` lancé à la main (et que
# le `.dockerignore` exclut volontairement du contexte).

USER foxguard

EXPOSE 8090

ENV RUST_LOG=foxguard_manager=info,warn

CMD [ "./foxguard-manager" ]
