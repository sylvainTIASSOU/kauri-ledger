-- Create accounts table
CREATE TABLE IF NOT EXISTS accounts (
    code VARCHAR(255) PRIMARY KEY,
    account_type VARCHAR(50) NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Ensure account type is valid (matches Rust enum)
ALTER TABLE accounts ADD CONSTRAINT chk_account_type 
CHECK (account_type IN ('Asset', 'Liability', 'Equity', 'Revenue', 'Expense'));