
-- Security roles must be provisioned before this migration.

-- Assign ownership to the trusted, non-login migration role.
ALTER SCHEMA ledger OWNER TO ledger_migrator;

ALTER TABLE ledger.ledgers OWNER TO ledger_migrator;
ALTER TABLE ledger.accounts OWNER TO ledger_migrator;
ALTER TABLE ledger.account_balances OWNER TO ledger_migrator;
ALTER TABLE ledger.transactions OWNER TO ledger_migrator;
ALTER TABLE ledger.entries OWNER TO ledger_migrator;
ALTER TABLE ledger.transaction_seals OWNER TO ledger_migrator;
ALTER TABLE ledger.period_locks OWNER TO ledger_migrator;

-- Security-definer functions must not remain superuser-owned.
ALTER FUNCTION ledger.guard_entry_insertion()
    OWNER TO ledger_migrator;

ALTER FUNCTION ledger.assert_transaction_balanced()
    OWNER TO ledger_migrator;

ALTER FUNCTION ledger.assert_period_open()
    OWNER TO ledger_migrator;

ALTER FUNCTION ledger.close_period(UUID, TIMESTAMPTZ)
    OWNER TO ledger_migrator;

ALTER FUNCTION ledger.reject_ledger_mutation()
    OWNER TO ledger_migrator;

-- Revoke all previously assigned access.
REVOKE ALL ON SCHEMA ledger
    FROM PUBLIC, ledger_app, ledger_admin, ledger_readonly;

REVOKE ALL ON ALL TABLES IN SCHEMA ledger
    FROM PUBLIC, ledger_app, ledger_admin, ledger_readonly;

REVOKE ALL ON ALL SEQUENCES IN SCHEMA ledger
    FROM PUBLIC, ledger_app, ledger_admin, ledger_readonly;

REVOKE ALL ON ALL FUNCTIONS IN SCHEMA ledger
    FROM PUBLIC, ledger_app, ledger_admin, ledger_readonly;

-- Schema visibility does not grant access to table contents.
GRANT USAGE ON SCHEMA ledger
    TO ledger_app, ledger_admin, ledger_readonly;

-- ledger_app deliberately receives no direct table privileges.
-- Tenant-scoped operations will use audited functions later.

-- Administration can inspect ledger status and period closures.
GRANT SELECT ON
    ledger.ledgers,
    ledger.period_locks
TO ledger_admin;

GRANT UPDATE (status) ON ledger.ledgers
    TO ledger_admin;

GRANT EXECUTE
    ON FUNCTION ledger.close_period(UUID, TIMESTAMPTZ)
    TO ledger_admin;

-- The audit role is privileged and must never be exposed
-- to ordinary tenant-facing application connections.
GRANT SELECT ON
    ledger.ledgers,
    ledger.accounts,
    ledger.account_balances,
    ledger.transactions,
    ledger.entries,
    ledger.transaction_seals,
    ledger.period_locks
TO ledger_readonly;

-- Prevent untrusted role access to creating public objects.
REVOKE CREATE ON SCHEMA public FROM PUBLIC;

-- Future functions created by the migration role must not
-- automatically become executable by PUBLIC.
ALTER DEFAULT PRIVILEGES FOR ROLE ledger_migrator
    REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
