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
