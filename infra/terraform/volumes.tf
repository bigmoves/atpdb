# ATPDB data volume - disabled, using local disk instead
# resource "hcloud_volume" "data" {
#   name     = "${local.name_prefix}-data"
#   size     = var.data_volume_size
#   location = var.location
#   format   = "ext4"
#   labels   = local.common_labels
# }

# Volume attachment
# resource "hcloud_volume_attachment" "data" {
#   volume_id = hcloud_volume.data.id
#   server_id = hcloud_server.main.id
#   automount = false
# }
