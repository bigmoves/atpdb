variable "hcloud_token" {
  description = "Hetzner Cloud API token"
  type        = string
  sensitive   = true
}

variable "environment" {
  description = "Environment name"
  type        = string
  default     = "production"
}

variable "server_type" {
  description = "Hetzner server type"
  type        = string
  default     = "cx22" # 2 vCPU, 4GB RAM - sufficient for atpdb
}

variable "location" {
  description = "Hetzner datacenter location"
  type        = string
  default     = "nbg1" # Nuremberg, Germany
}

variable "ssh_keys" {
  description = "SSH key names to add to server"
  type        = list(string)
}

variable "domain" {
  description = "Domain for the server"
  type        = string
}

variable "grafana_password" {
  description = "Password for Grafana admin"
  type        = string
  sensitive   = true
}

variable "github_repo" {
  description = "GitHub repository for atpdb releases"
  type        = string
  default     = "bigmoves/atpdb"
}

variable "cloudflare_api_token" {
  description = "Cloudflare API token with DNS edit permissions"
  type        = string
  sensitive   = true
}

variable "cloudflare_zone_id" {
  description = "Cloudflare Zone ID for the domain"
  type        = string
}

variable "subdomain" {
  description = "Subdomain for the DNS record (e.g., 'atpdb' for atpdb.example.com)"
  type        = string
  default     = "atpdb"
}

variable "data_volume_size" {
  description = "Size of data volume in GB"
  type        = number
  default     = 50
}
