output "server_ipv4" {
  description = "Public IPv4 address of the server"
  value       = hcloud_server.main.ipv4_address
}

output "server_ipv6" {
  description = "Public IPv6 address of the server"
  value       = hcloud_server.main.ipv6_address
}

output "server_name" {
  description = "Name of the server"
  value       = hcloud_server.main.name
}

output "data_volume_id" {
  description = "ID of the data volume"
  value       = hcloud_volume.data.id
}
