#!/bin/bash
set -euo pipefail

echo "Installing Morph Bang Dependencies..."
sudo pacman -S --needed rustup inotify-tools libvips imagemagick pandoc ffmpeg libnotify texlive-bin texlive-xetex poppler

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
sudo tee /etc/systemd/system/morph-bang.service <<EOF
[Unit]
Description=Universal File Data Morphing Daemon (Morph Bang)
After=network.target

[Service]
User=root
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
