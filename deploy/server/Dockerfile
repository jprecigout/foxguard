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

COPY crates ./crates

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

# Bundle de l'interface React. Tant que `ui/` n'est pas construit, ce dossier
# reste vide et le manager se contente de servir son API.
COPY --chown=foxguard:foxguard ui/dist* /app/ui/dist/

USER foxguard

EXPOSE 8090

ENV RUST_LOG=foxguard_manager=info,warn

CMD [ "./foxguard-manager" ]
