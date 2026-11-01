#!/bin/bash
set -euo pipefail

ENGINE="native"
for arg in "$@"; do
  case "$arg" in
    --docker) ENGINE="docker" ;;
    --native) ENGINE="native" ;;
    *) echo "Usage: $0 [--native|--docker]"; exit 1 ;;
  esac
done

DOCKER_IMAGE="${MORPH_DOCKER_IMAGE:-ghcr.io/c4illin/convertx:latest}"

echo "Installing Morph Bang Dependencies (engine: $ENGINE)..."
if [ "$ENGINE" = "docker" ]; then
  # Conversion tools live in the toolbox container; only the watcher, file
  # detection and notifications run on the host.
  sudo pacman -S --needed rustup inotify-tools file libnotify docker
  sudo systemctl enable --now docker.service
  echo "Pulling toolbox image $DOCKER_IMAGE..."
  sudo docker pull "$DOCKER_IMAGE"
else
  sudo pacman -S --needed rustup inotify-tools file libvips imagemagick pandoc ffmpeg libnotify texlive-bin texlive-xetex poppler ghostscript
  # Registry converters available in the official repos. Missing ones are skipped at runtime.
  # Not packaged here (use --docker or install manually): dasel, vtracer, markitdown, msgconvert.
  sudo pacman -S --needed graphicsmagick inkscape resvg libjxl libheif calibre libreoffice-fresh assimp potrace texlive-binextra || true
fi

echo "Setting up Rust toolchain..."
rustup toolchain install stable --profile minimal
rustup default stable

echo "Configuring Inotify limits..."
echo "fs.inotify.max_user_watches=524288" | sudo tee /etc/sysctl.d/99-inotify.conf
sudo sysctl --system

echo "Building morph-bang..."
rustup run stable cargo build --release

echo "Stopping service before binary update..."
sudo systemctl stop morph-bang.service 2>/dev/null || true

echo "Installing binary..."
sudo cp target/release/morph-bang /usr/local/bin/morph-bang
sudo chmod +x /usr/local/bin/morph-bang

echo "Creating Systemd service..."
if [ "$ENGINE" = "docker" ]; then
  UNIT_AFTER="network.target docker.service"
  UNIT_REQUIRES="Requires=docker.service"
  UNIT_ENV="Environment=MORPH_ENGINE=docker
Environment=MORPH_DOCKER_IMAGE=$DOCKER_IMAGE"
else
  UNIT_AFTER="network.target"
  UNIT_REQUIRES=""
  UNIT_ENV="Environment=MORPH_ENGINE=native"
fi

sudo tee /etc/systemd/system/morph-bang.service <<EOF
[Unit]
Description=Universal File Data Morphing Daemon (Morph Bang)
After=$UNIT_AFTER
$UNIT_REQUIRES

[Service]
User=root
$UNIT_ENV
ExecStart=/usr/local/bin/morph-bang
Restart=always
Nice=-15
IOSchedulingClass=realtime

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable --now morph-bang.service
echo "Morph Bang is now active. Rename to .!<ext> to morph a file."
