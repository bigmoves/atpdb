# DNS A record pointing to Hetzner server
resource "cloudflare_record" "atpdb" {
  zone_id = var.cloudflare_zone_id
  name    = var.subdomain
  content = hcloud_server.main.ipv4_address
  type    = "A"
  ttl     = 300
  proxied = false # Direct connection for WebSocket support
}

# AAAA record for IPv6
resource "cloudflare_record" "atpdb_ipv6" {
  zone_id = var.cloudflare_zone_id
  name    = var.subdomain
  content = hcloud_server.main.ipv6_address
  type    = "AAAA"
  ttl     = 300
  proxied = false
}
