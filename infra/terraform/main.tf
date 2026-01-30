locals {
  name_prefix = "atpdb-${var.environment}"

  common_labels = {
    environment = var.environment
    project     = "atpdb"
    managed_by  = "terraform"
  }
}

# Reference SSH keys by name
data "hcloud_ssh_key" "deploy" {
  for_each = toset(var.ssh_keys)
  name     = each.value
}
