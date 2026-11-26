
-- Create tenant-scoped accounts with validated accounting semantics.
CREATE TABLE ledger.accounts (
    ledger_id UUID NOT NULL,
    id UUID NOT NULL,

    code TEXT COLLATE "C" NOT NULL,
    kind TEXT NOT NULL,
    normal_side CHAR(1) NOT NULL,
    allow_negative BOOLEAN NOT NULL DEFAULT FALSE,

    name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',

    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    closed_at TIMESTAMPTZ,

    CONSTRAINT accounts_pk
        PRIMARY KEY (ledger_id, id),

    CONSTRAINT accounts_ledger_fk
        FOREIGN KEY (ledger_id)
        REFERENCES ledger.ledgers (id)
        ON DELETE RESTRICT,

    CONSTRAINT accounts_ledger_code_unique
        UNIQUE (ledger_id, code),

    CONSTRAINT accounts_balance_policy_unique
        UNIQUE (ledger_id, id, normal_side, allow_negative),

    CONSTRAINT accounts_code_valid
        CHECK (
            code ~ '^[a-z0-9][a-z0-9:_-]{0,127}$'
        ),

    CONSTRAINT accounts_kind_valid
        CHECK (
            kind IN (
                'asset',
                'liability',
                'equity',
                'revenue',
                'expense'
            )
        ),

    CONSTRAINT accounts_normal_side_valid
        CHECK (normal_side IN ('D', 'C')),

    CONSTRAINT accounts_kind_normal_side_match
        CHECK (
            (
                kind IN ('asset', 'expense')
                AND normal_side = 'D'
            )
            OR
            (
                kind IN ('liability', 'equity', 'revenue')
                AND normal_side = 'C'
            )
        ),

    CONSTRAINT accounts_name_valid
        CHECK (
            char_length(name) BETWEEN 1 AND 200
            AND char_length(btrim(name)) > 0
        ),

    CONSTRAINT accounts_status_valid
        CHECK (status IN ('active', 'closed'))
);

-- Store cumulative amounts rather than a single signed balance.
CREATE TABLE ledger.account_balances (
    ledger_id UUID NOT NULL,
    account_id UUID NOT NULL,

    normal_side CHAR(1) NOT NULL,
    allow_negative BOOLEAN NOT NULL,

    debit_total BIGINT NOT NULL DEFAULT 0,
    credit_total BIGINT NOT NULL DEFAULT 0,
    reserved_total BIGINT NOT NULL DEFAULT 0,

    version BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT account_balances_pk
        PRIMARY KEY (ledger_id, account_id),

    CONSTRAINT account_balances_account_fk
        FOREIGN KEY (
            ledger_id,
            account_id,
            normal_side,
            allow_negative
        )
        REFERENCES ledger.accounts (
            ledger_id,
            id,
            normal_side,
            allow_negative
        )
        ON UPDATE CASCADE
        ON DELETE RESTRICT,

    CONSTRAINT account_balances_debit_total_valid
        CHECK (
            debit_total BETWEEN 0 AND 4000000000000000000
        ),

    CONSTRAINT account_balances_credit_total_valid
        CHECK (
            credit_total BETWEEN 0 AND 4000000000000000000
        ),

    CONSTRAINT account_balances_reserved_total_valid
        CHECK (
            reserved_total BETWEEN 0 AND 4000000000000000000
        ),

    CONSTRAINT account_balances_version_valid
        CHECK (version >= 0),

    CONSTRAINT account_balances_posted_nonnegative
        CHECK (
            allow_negative
            OR (
                CASE
                    WHEN normal_side = 'D'
                        THEN debit_total - credit_total
                    ELSE credit_total - debit_total
                END
            ) >= 0
        ),

    CONSTRAINT account_balances_available_nonnegative
        CHECK (
            allow_negative
            OR (
                CASE
                    WHEN normal_side = 'D'
                        THEN debit_total - credit_total
                    ELSE credit_total - debit_total
                END
            ) - reserved_total >= 0
        )
);
