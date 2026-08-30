# ==========================================
# 1. STAGE DE COMPILATION (BUILD)
# ==========================================
# pour avoir une version de rust 1.91 slim
FROM rust:1-slim AS builder 

# Dépendances nécessaires à la compilation (V4L2, C toolchain)
RUN apt-get update && apt-get install -y \
    pkg-config \
    libv4l-dev \
    libclang-dev \
    build-essential \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Mise en cache des dépendances Cargo
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release --locked
RUN rm -rf src

# Copie du code source et des assets
COPY src ./src
COPY static ./static
COPY assets ./assets

# Compilation finale du binaire
RUN touch src/main.rs && cargo build --release

# ==========================================
# 2. STAGE D'EXÉCUTION (RUNTIME SÉCURISÉ)
# ==========================================
FROM debian:bookworm-slim

# Dépendances d'exécution
RUN apt-get update && apt-get install -y \
    libv4l-0 \
    ca-certificates \
    tzdata \
    && rm -rf /var/lib/apt/lists/*

# Définition du fuseau horaire (Europe/Paris)
ENV TZ=Europe/Paris

# 🔒 CRÉATION DE L'UTILISATEUR NON-ROOT SÉCURISÉ
# 1. On crée un utilisateur/groupe 'foxguard' sans privilèges
# 2. On l'ajoute au groupe système 'video' (GID 44 en général) pour l'accès aux flux V4L2 (/dev/video*)
RUN groupadd -g 10001 foxguard && \
    useradd -u 10001 -g foxguard -G video -m -s /bin/false foxguard

WORKDIR /app

# Création du répertoire d'enregistrement et attribution des permissions
RUN mkdir -p /app/src/vision /app/output_record && \
    chown -R foxguard:foxguard /app

# Copie des artefacts compilés et fichiers nécessaires depuis le stage 'builder'
COPY --from=builder --chown=foxguard:foxguard /app/target/release/foxguard /app/foxguard
COPY --from=builder --chown=foxguard:foxguard /app/static /app/static
COPY --chown=foxguard:foxguard config.toml /app/config.toml
COPY --chown=foxguard:foxguard src/vision/yolov8n.onnx /app/src/vision/yolov8n.onnx

# 🔒 BASCULEMENT SUR L'UTILISATEUR NON-ROOT
USER foxguard

# Exposition du port Web / WebSocket
EXPOSE 8080

ENV RUST_LOG=info

CMD [ "./foxguard"]