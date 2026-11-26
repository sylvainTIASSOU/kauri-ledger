
-- Isolate legacy accounting storage from tenant-facing roles.
-- Preserve existing records and trusted maintenance workflows.
-- The ledger schema is the authoritative multi-tenant engine.

REVOKE ALL PRIVILEGES
ON TABLE
    public.accounts,
    public.transactions,
    public.entries,
    public.balances
FROM
    PUBLIC,
    ledger_app,
    ledger_admin,
    ledger_readonly;

-- Legacy trigger functions are not public application APIs.
REVOKE ALL PRIVILEGES
ON FUNCTION public.prevent_entry_modification()
FROM
    PUBLIC,
    ledger_app,
    ledger_admin,
    ledger_readonly;

-- Prevent untrusted object creation in either accounting namespace.
REVOKE CREATE ON SCHEMA public
FROM
    PUBLIC,
    ledger_app,
    ledger_admin,
    ledger_readonly;

REVOKE CREATE ON SCHEMA ledger
FROM
    PUBLIC,
    ledger_app,
    ledger_admin,
    ledger_readonly;
