# Image de la CAMÉRA FoxGuard (crates/camera), déployée sur le Raspberry Pi.
#
# À construire depuis la RACINE du workspace :
#   docker build -f deploy/camera/Dockerfile -t foxguard-camera:rpi4 .
#
# ==========================================
# 1. STAGE DE COMPILATION (BUILD)
# ==========================================
# pour avoir une version de rust 1.91 slim
FROM rust:1-slim AS builder 

# Dépendances nécessaires à la compilation (V4L2, C toolchain, TLS pour
# l'envoi d'e-mail via lettre : libssl-dev pour openssl-sys/native-tls)
RUN apt-get update && apt-get install -y \
    pkg-config \
    libv4l-dev \
    libclang-dev \
    libssl-dev \
    build-essential \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Mise en cache des dépendances Cargo (workspace)
#
# Cargo a besoin des manifestes de TOUS les membres du workspace pour résoudre
# le graphe, même si on n'en construit qu'un : on les copie donc tous, avec
# des sources factices, pour que cette couche ne soit invalidée que par un
# changement de dépendance et pas par un changement de code.
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

# `-p foxguard-camera` : SEUL le sous-graphe de dépendances de la caméra est
# compilé. Les dépendances propres au manager (dont son éventuel TLS, et donc
# `ring` et son assembleur par architecture qui échoue sous QEMU) figurent
# dans le Cargo.lock partagé sans jamais être construites ici.
RUN cargo build --release --locked -p foxguard-camera
RUN rm -rf crates/camera/src crates/manager/src crates/protocol/src

# Copie du code source et des assets
COPY crates ./crates

# Compilation finale du binaire
RUN cargo build --release -p foxguard-camera

# ==========================================
# 2. STAGE D'EXÉCUTION (RUNTIME SÉCURISÉ)
# ==========================================
FROM debian:bookworm-slim

# Dépendances d'exécution (libssl3 : lib OpenSSL partagée requise par
# native-tls, utilisé par lettre pour l'envoi d'e-mail en TLS)
RUN apt-get update && apt-get install -y \
    libv4l-0 \
    libssl3 \
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

# Création des répertoires de données et attribution des permissions.
#
# L'arborescence `crates/camera/models` est reproduite telle quelle parce que
# les chemins de `config.toml` sont relatifs au répertoire de travail : le
# MÊME fichier de configuration fonctionne ainsi en développement (depuis la
# racine du workspace) et dans l'image.
RUN mkdir -p /app/crates/camera/models /app/output_record /app/known_faces && \
    chown -R foxguard:foxguard /app

# Copie des artefacts compilés et fichiers nécessaires depuis le stage 'builder'
COPY --from=builder --chown=foxguard:foxguard /app/target/release/foxguard-camera /app/foxguard-camera
# `static/` est servi à l'exécution par ServeDir (icônes de l'interface) :
# il doit être présent dans l'image, pas seulement à la compilation.
COPY --from=builder --chown=foxguard:foxguard /app/crates/camera/static /app/static
COPY --chown=foxguard:foxguard config.toml /app/config.toml
# Les 3 modèles ONNX (YOLOv8, YuNet, ArcFace)
COPY --chown=foxguard:foxguard crates/camera/models /app/crates/camera/models

# 🔒 BASCULEMENT SUR L'UTILISATEUR NON-ROOT
USER foxguard

# Exposition du port Web / WebSocket
EXPOSE 8080

ENV RUST_LOG=info

CMD [ "./foxguard-camera" ]