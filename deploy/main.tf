terraform {
  required_version = "~> 1.7"

  required_providers {
    google     = "~> 4.0"
  }
}

variable "project" {
  description = "cnzdt-devops"
}

variable "region" {
  description = "asia-northeast1"
}

provider "google" {
  project = var.project
  region  = var.region
}

variable "gcp_services" {
  description = "APIs required for the project"
  type        = list(string)
  default = [
    "cloudbuild.googleapis.com",
    "compute.googleapis.com",
    "dns.googleapis.com",
    "iam.googleapis.com",
    "run.googleapis.com",
    "secretmanager.googleapis.com",
    "storage.googleapis.com",
    "servicenetworking.googleapis.com"
  ]
}

resource "google_project_service" "gcp_services" {
  for_each = toset(var.gcp_services)
  project  = var.project
  service  = each.key
}