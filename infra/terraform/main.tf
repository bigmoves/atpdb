locals {
  # Include workspace name to make resources unique across workspaces
  workspace_suffix = terraform.workspace == "default" ? "" : "-${terraform.workspace}"
  name_prefix      = "atpdb-${var.environment}${local.workspace_suffix}"

  common_labels = {
    environment = var.environment
    workspace   = terraform.workspace
    project     = "atpdb"
    managed_by  = "terraform"
  }
}

# Reference SSH keys by name
data "hcloud_ssh_key" "deploy" {
  for_each = toset(var.ssh_keys)
  name     = each.value
}
