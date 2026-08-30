# 🦊 FoxGuard

**FoxGuard** est un système de vidéosurveillance intelligent propulsé par l'IA et développé en Rust.

---

## 🚀 Fonctionnalités Principales

* **Détection d'objets par IA** : Analyse des flux vidéo en temps réel à l'aide de modèles YOLOv8 via la bibliothèque `tract-onnx`.
* **Interface Web de Contrôle** : Panneau de contrôle moderne intégré et servi via le framework web Axum (`static/controler.html`).
* **Streaming Vidéo en Direct** : Diffusion par WebSockets (`/ws`) avec une optimisation de type *pass-through* (transmission directe du buffer MJPEG sans décodage/ré-encodage CPU superflu lorsque la détection est inactive).
* **Enregistrement Vidéo** : Sauvegarde des flux en fichiers `.mjpeg` activable dynamiquement depuis l'interface web.
* **Alertes par E-mail** : Notification automatique d'événements gérée par un module d'e-mail avec gestion de délai (*cooldown*).
* **Overlay Graphique Natif** : Dessin de boîtes englobantes (*bounding boxes*) et de libellés textuels optimisés grâce à la police bitmap `font8x8`.

---

## 📂 Structure du Projet

* **`src/main.rs`** : Point d'entrée de l'application, affichage de la bannière de démarrage et initialisation des services.
* **`src/camera.rs`** : Gestion de la boucle de capture V4L2, application du saut de frames (*frame skipping*), gestion de l'overlay d'IA et diffusion WebSocket.
* **`src/api.rs`** : Configuration du routage Axum, service de la page HTML de contrôle et gestion des commandes JSON entrantes.
* **`src/vision/`** : Module d'IA gérant le chargement du modèle ONNX, le prétraitement et l'inférence des objets.
* **`static/controler.html`** : Interface utilisateur web avec affichage vidéo et interrupteurs interactifs.

---

## 🛠️ Compilation et Lancement

Pour lancer l'application en mode optimisé (recommandé pour les performances de l'inférence IA) :

```bash
cargo run --release
```

---

## 🚀 Commande de Build et Lancement Docker

### Construire l'image Docker 

```bash
docker build -t foxguard:latest .
```

### Lancer le conteneur avec accès à la caméra locale (/dev/video0)

Comme l'application interagit avec la caméra webcam V4L2 locale, il faut lui passer le périphérique vidéo (--device) et persister le dossier des enregistrements (-v)

```bash
docker run -d \
  --name foxguard_app \
  --device=/dev/video0:/dev/video0 \
  -p 8080:8080 \
  -v $(pwd)/output_record:/app/output_record \
  foxguard:latest
```

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
docker buildx build --platform linux/arm64 -t foxguard:rpi4 --output type=docker,dest=foxguard_rpi4.tar .
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
docker buildx build --platform linux/arm64 -t jprecigout/foxguard:rpi4 --push .
```

sur le Raspberry Pi

```bash
docker pull jprecigout/foxguard:rpi4
```

## 🛠️ Modification de la configuration du rapsberry pour activer le pilote V4L2 Legacy
Il faut configurer le Raspberry Pi pour qu'il utilise le contrôleur vidéo hérité compatible V4L2 natif.

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
  --restart unless-stopped \
  --privileged \
  -v /dev:/dev \
  -p 8080:8080 \
  -v $(pwd)/output_record:/app/output_record \
  foxguard:rpi4
```
---

## 🌐 Utilisation de l'Interface Web

Ouvrez votre navigateur web et rendez-vous sur l'adresse du serveur (par exemple : http://localhost:8080 ou http://foxguard.local:8080).

Le flux vidéo s'établit automatiquement via WebSocket.

Utilisez les interrupteurs interactifs pour :

Activer ou désactiver la Détection IA en temps réel.

Démarrer ou arrêter l'Enregistrement Vidéo local.

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
