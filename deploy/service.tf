locals {
  #  Replace this to match your container image
  artifacts = {
    image-url = "docker.io/kingjiyang/atlas-txn-sender:d555451-amd64"
  }
  service-name = "atlas-txn-sender"
}

resource "google_cloud_run_service" "service" {
  name     = local.service-name
  location = var.region

  template {
    spec {
      timeout_seconds       = 600
      containers {
        image = "${local.artifacts.image-url}"
        ports {
          name = "http1"
          protocol = "TCP"
          container_port = 4040
        }
        resources {
          limits = {
            cpu = "2000m"
            memory = "4Gi"
          }
          requests = {
            cpu = "1000m"
            memory = "1Gi"
          }
        }
        env{
          name = "RUST_BACKTRACE"
          value = 1
        }
        env {
          name = "RUST_LOG"
          value = "ERROR"
        }
        env {
          name = "RPC_URL"
          value = "https://api.mainnet-beta.solana.com"
        }
        env {
          name = "GRPC_URL"
          value = "http://api.rpcpool.com"
        }
        env {
          name = "X_TOKEN"
          value_from {
            secret_key_ref {
              name = "X_TOKEN"
              key = "1" # version of the secret
            }
          }
        }
        env {
          name = "DD_API_KEY"
          value_from {
            secret_key_ref {
              name = "DD_API_KEY"
              key = "1" # version of the secret
            }
          }
        }
        env {
          name = "DD_SITE"
          value = local.service-name
        }
        env {
          name = "DD_SERVICE"
          value = "datadog-atlas-txn-sender-run"
        }
        env {
          name = "DD_ENV"
          value = "datadog-atlas-txn-sender"
        }
        env {
          name = "DD_VERSION"
          value = "1"
        }
        env {
          name = "DD_LOGS_ENABLED"
          value = true
        }
        env {
          name = "TPU_CONNECTION_POOL_SIZE"
          value = "4"
        }
        env {
          name = "NUM_LEADERS"
          value = "10"
        }
        env{
          name = "IDENTITY_KEYPAIR_FILE"
          value = "/solana/account.json"
        }
        volume_mounts {
          name = "account-id"
          mount_path = "/solana"
        }
      }

      volumes {
        name = "account-id"
        secret {
          # secret_id = "projects/480679768042/secrets/IDENTITY_KEYPAIR_FILE"
          secret_name = "IDENTITY_KEYPAIR_FILE"
          items {
            key  = "latest"
            path = "account.json"
          }
        }
      }
    }
  }

  metadata {
    annotations = {
      #    This sets the service to only allow all traffic
      "run.googleapis.com/ingress" = "all"
      "autoscaling.knative.dev/maxScale" = 1
      "run.googleapis.com/cpu-throttling" = false
    }
  }

  traffic {
    percent         = 100
    latest_revision = true
  }
  autogenerate_revision_name = true
}

# We're not using Cloud IAM for authentication in this example. If you're using it in your service however, you can
# delete the following noauth blocks
data "google_iam_policy" "noauth" {
  binding {
    role = "roles/run.invoker"
    members = [
      "allUsers",
    ]
  }
}

resource "google_cloud_run_service_iam_policy" "noauth" {
  location    = google_cloud_run_service.service.location
  project     = google_cloud_run_service.service.project
  service     = google_cloud_run_service.service.name
  policy_data = data.google_iam_policy.noauth.policy_data
}