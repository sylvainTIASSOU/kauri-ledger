-- Enable pgcrypto extension for gen_random_uuid() compatibility across PostgreSQL versions
CREATE EXTENSION IF NOT EXISTS "pgcrypto";

-- Create transactions table
CREATE TABLE IF NOT EXISTS transactions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    idempotency_key VARCHAR(255) UNIQUE NOT NULL,
    payload_hash VARCHAR(64) NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Create entries table
CREATE TABLE IF NOT EXISTS entries (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    transaction_id UUID NOT NULL REFERENCES transactions(id) ON DELETE CASCADE,
    account_code VARCHAR(255) NOT NULL REFERENCES accounts(code) ON DELETE RESTRICT,
    direction VARCHAR(10) NOT NULL,
    amount BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Ensure direction is valid
ALTER TABLE entries ADD CONSTRAINT chk_direction 
CHECK (direction IN ('Debit', 'Credit'));

-- Ensure amount is strictly positive (Property: amounts > 0)
ALTER TABLE entries ADD CONSTRAINT chk_amount_positive 
CHECK (amount > 0);

-- Indexes for faster balance calculations and lookups
CREATE INDEX idx_entries_account_code ON entries(account_code);
CREATE INDEX idx_entries_transaction_id ON entries(transaction_id);