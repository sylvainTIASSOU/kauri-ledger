BEGIN;

-- Create two independent development ledgers.
INSERT INTO ledger.ledgers (id, name)
VALUES
(
    '019bb000-0000-7000-8000-000000000001',
    'Kauri Development Ledger A'
),
(
    '019bb000-0000-7000-8000-000000000002',
    'Kauri Development Ledger B'
)
ON CONFLICT (id) DO NOTHING;

-- Ledger A: customer wallet.
SELECT ledger.create_account(
    '019bb000-0000-7000-8000-000000000001',
    '019bb000-0000-7000-8000-000000000011',
    'asset:wallet',
    'asset',
    'Customer Wallet A',
    false
)
WHERE NOT EXISTS (
    SELECT 1
    FROM ledger.accounts
    WHERE ledger_id = '019bb000-0000-7000-8000-000000000001'
      AND code = 'asset:wallet'
);

-- Ledger A: funding account.
SELECT ledger.create_account(
    '019bb000-0000-7000-8000-000000000001',
    '019bb000-0000-7000-8000-000000000012',
    'asset:funding',
    'asset',
    'Funding Account A',
    true
)
WHERE NOT EXISTS (
    SELECT 1
    FROM ledger.accounts
    WHERE ledger_id = '019bb000-0000-7000-8000-000000000001'
      AND code = 'asset:funding'
);

-- Ledger B: independent wallet.
SELECT ledger.create_account(
    '019bb000-0000-7000-8000-000000000002',
    '019bb000-0000-7000-8000-000000000021',
    'asset:wallet',
    'asset',
    'Customer Wallet B',
    false
)
WHERE NOT EXISTS (
    SELECT 1
    FROM ledger.accounts
    WHERE ledger_id = '019bb000-0000-7000-8000-000000000002'
      AND code = 'asset:wallet'
);

-- Ledger B: independent funding account.
SELECT ledger.create_account(
    '019bb000-0000-7000-8000-000000000002',
    '019bb000-0000-7000-8000-000000000022',
    'asset:funding',
    'asset',
    'Funding Account B',
    true
)
WHERE NOT EXISTS (
    SELECT 1
    FROM ledger.accounts
    WHERE ledger_id = '019bb000-0000-7000-8000-000000000002'
      AND code = 'asset:funding'
);

COMMIT;