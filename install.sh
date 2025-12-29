#!/bin/bash
echo "Installing Morph Dependencies..."
sudo pacman -S --needed inotify-tools libvips imagemagick pandoc ffmpeg libnotify texlive-bin texlive-xetex poppler

echo "Configuring Inotify limits..."
echo "fs.inotify.max_user_watches=524288" | sudo tee /etc/sysctl.d/99-inotify.conf
sudo sysctl --system

echo "Setting up script..."
sudo cp morph /usr/local/bin/morph
sudo chmod +x /usr/local/bin/morph

echo "Creating Systemd service..."
sudo tee /etc/systemd/system/morph.service <<EOF
[Unit]
Description=Universal File Data Morphing Daemon
After=network.target

[Service]
User=root
ExecStart=/bin/bash /usr/local/bin/morph
Restart=always
Nice=-15
IOSchedulingClass=realtime

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable --now morph.service
echo "Morph is now active. Try renaming a file!"