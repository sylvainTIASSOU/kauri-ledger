-- Create balances table
CREATE TABLE IF NOT EXISTS balances (
    account_code VARCHAR(255) PRIMARY KEY REFERENCES accounts(code) ON DELETE CASCADE,
    balance BIGINT NOT NULL DEFAULT 0,
    version BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Trigger function to prevent direct updates or deletions of entries (Immutability)
CREATE OR REPLACE FUNCTION prevent_entry_modification()
RETURNS TRIGGER AS $$
BEGIN
    RAISE EXCEPTION 'ENTRIES_IMMUTABLE: Ledger entries cannot be updated or deleted. Use a reversing transaction to correct errors.';
END;
$$ LANGUAGE plpgsql;

-- Apply trigger to entries table
CREATE TRIGGER trg_prevent_entry_modification
BEFORE UPDATE OR DELETE ON entries
FOR EACH ROW
EXECUTE FUNCTION prevent_entry_modification();