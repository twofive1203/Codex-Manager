ALTER TABLE request_logs ADD COLUMN client_ip TEXT;

CREATE INDEX IF NOT EXISTS idx_request_logs_client_ip_created_at
  ON request_logs(client_ip, created_at DESC);
