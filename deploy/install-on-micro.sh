#!/bin/bash
# Runs as root on micro1 (needs MDPW env and /tmp/mydrive binary).
set -e
id mydrive >/dev/null 2>&1 || useradd --system --home-dir /srv/mydrive --shell /usr/sbin/nologin mydrive
install -m 755 /tmp/mydrive /usr/local/bin/mydrive && rm -f /tmp/mydrive
mkdir -p /srv/mydrive && chown mydrive:mydrive /srv/mydrive && chmod 700 /srv/mydrive
printf '%s\n' "$MDPW" | sudo -u mydrive MYDRIVE_DATA=/srv/mydrive /usr/local/bin/mydrive set-password
cat > /etc/systemd/system/mydrive.service <<'U'
[Unit]
Description=MyDrive personal cloud drive
After=network-online.target
Wants=network-online.target
[Service]
User=mydrive
Group=mydrive
Environment=MYDRIVE_DATA=/srv/mydrive MYDRIVE_LISTEN=10.20.0.19:8090 MYDRIVE_QUOTA_GB=40 MYDRIVE_USER=sreekar
ExecStart=/usr/local/bin/mydrive
Restart=always
RestartSec=3
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/srv/mydrive
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
RestrictNamespaces=yes
LockPersonality=yes
CapabilityBoundingSet=
SystemCallArchitecture=native
MemoryMax=300M
[Install]
WantedBy=multi-user.target
U
sed -i 's/^SystemCallArchitecture=/SystemCallArchitectures=/' /etc/systemd/system/mydrive.service
systemctl daemon-reload && systemctl enable --now mydrive >/dev/null 2>&1 && systemctl restart mydrive
iptables -C INPUT -s 10.20.0.246/32 -p tcp --dport 8090 -j ACCEPT 2>/dev/null || iptables -I INPUT 5 -s 10.20.0.246/32 -p tcp --dport 8090 -j ACCEPT
netfilter-persistent save >/dev/null 2>&1 || true
grep -q '^\[Session\]' /etc/cockpit/cockpit.conf || printf '\n[Session]\nIdleTimeout=60\n' >> /etc/cockpit/cockpit.conf
sleep 2
echo "service: $(systemctl is-active mydrive)  mem: $(ps -o rss= -C mydrive) KB  disk free: $(df -h /srv | awk 'NR==2{print $4}')"
journalctl -u mydrive -n 2 -o cat
