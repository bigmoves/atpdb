resource "hcloud_server" "main" {
  name        = "${local.name_prefix}-server"
  server_type = var.server_type
  location    = var.location
  image       = "ubuntu-24.04"
  labels      = local.common_labels

  ssh_keys = [for key in data.hcloud_ssh_key.deploy : key.id]

  firewall_ids = [hcloud_firewall.main.id]

  public_net {
    ipv4_enabled = true
    ipv6_enabled = true
  }

  user_data = templatefile("${path.module}/cloud-init.yaml", {
    hostname                = "${local.name_prefix}-server"
    domain                  = var.domain
    grafana_password        = var.grafana_password
    github_repo             = var.github_repo
    atpdb_mode              = var.atpdb_mode
    atpdb_relay             = var.atpdb_relay
    atpdb_signal_collection = var.atpdb_signal_collection
    atpdb_collections       = var.atpdb_collections
    atpdb_indexes           = var.atpdb_indexes
    atpdb_search_fields     = var.atpdb_search_fields
    atpdb_cache_size_mb     = var.atpdb_cache_size_mb
    atpdb_log_level         = var.atpdb_log_level
    atpdb_sync_parallelism  = var.atpdb_sync_parallelism
  })

  lifecycle {
    ignore_changes = [user_data, ssh_keys]
  }
}
