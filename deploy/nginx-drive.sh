#!/bin/bash
# Runs as root on arm1: HTTPS front door for MyDrive at drive-<ip>.sslip.io
set -e
P=150-230-175-116.sslip.io; D=drive-$P
grep -q " $D;" /etc/nginx/sites-available/acme || sed -i "s/;\s*location \/\.well-known/ $D; location \/.well-known/" /etc/nginx/sites-available/acme
grep -o "server_name[^;]*;" /etc/nginx/sites-available/acme
systemctl reload nginx
NAMES=$(grep -o "server_name[^;]*" /etc/nginx/sites-available/acme | sed 's/server_name//' | tr ' ' '\n' | grep . | sed 's/^/-d /' | tr '\n' ' ')
certbot certonly --webroot -w /var/www/html --cert-name panel-$P --expand $NAMES --agree-tos -n 2>&1 | grep -iE "success|error|problem" | head -3
grep -q "server_name $D;" /etc/nginx/sites-available/secure || cat >> /etc/nginx/sites-available/secure <<N
server {
  listen 443 ssl http2; server_name $D;
  ssl_certificate /etc/letsencrypt/live/panel-$P/fullchain.pem; ssl_certificate_key /etc/letsencrypt/live/panel-$P/privkey.pem;
  add_header Strict-Transport-Security "max-age=31536000" always;
  client_max_body_size 0;
  proxy_request_buffering off; proxy_buffering off;
  proxy_read_timeout 1h; proxy_send_timeout 1h;
  location = /api/login { limit_req zone=login burst=5 nodelay; proxy_pass http://10.20.0.19:8090; proxy_set_header Host \$host; proxy_set_header X-Forwarded-For \$remote_addr; }
  location / { proxy_pass http://10.20.0.19:8090; proxy_http_version 1.1; proxy_set_header Host \$host; proxy_set_header X-Forwarded-For \$remote_addr; proxy_set_header Connection ""; }
}
N
grep -q '^\[Session\]' /etc/cockpit/cockpit.conf || printf '\n[Session]\nIdleTimeout=60\n' >> /etc/cockpit/cockpit.conf
nginx -t 2>&1 | tail -1 && systemctl reload nginx
echo "drive https: $(curl -s -o /dev/null -w '%{http_code}' https://$D/)  api/me: $(curl -s -o /dev/null -w '%{http_code}' https://$D/api/me)"
