
-- Create the dedicated schema for the ledger domain.
-- Existing tables in the public schema remain unchanged.
CREATE SCHEMA ledger;

-- Prevent implicit schema access by PUBLIC.
-- Runtime roles will receive explicit privileges in a later migration.
REVOKE ALL ON SCHEMA ledger FROM PUBLIC;

-- Each ledger represents an independent accounting boundary.
CREATE TABLE ledger.ledgers (
    id UUID PRIMARY KEY,

    name TEXT NOT NULL,

    currency CHAR(3) NOT NULL DEFAULT 'XOF',

    status TEXT NOT NULL DEFAULT 'active',

    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT ledgers_name_valid
        CHECK (
            char_length(name) BETWEEN 1 AND 200
            AND char_length(btrim(name)) > 0
        ),

    CONSTRAINT ledgers_currency_xof
        CHECK (currency = 'XOF'),

    CONSTRAINT ledgers_status_valid
        CHECK (status IN ('active', 'frozen'))
);
