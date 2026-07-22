-- Encryption at rest for ticket signing keys.
--
-- `ticket_keys.secret` held the raw Ed25519 seed. A database dump — a
-- backup in object storage, a stolen replica, an over-broad read grant —
-- was therefore enough to forge a valid boarding pass for any trip,
-- indefinitely, and the forgery would verify at every offline gate.
--
-- The seed is now sealed with a key that lives outside the database
-- (LULAN_TICKET_KEY_ENCRYPTION_KEY), so a dump alone is inert.
--
-- Both shapes stay readable. `encryption IS NULL` means the row predates
-- this, or the deployment has not configured a wrapping key; existing rows
-- keep working untouched, and are sealed in place at boot once one is set.
-- Public halves are never encrypted, which is what makes losing the
-- wrapping key survivable: issued tickets go on verifying and the operator
-- only has to rotate.

ALTER TABLE ticket_keys
    ADD COLUMN encryption text,
    ADD COLUMN nonce      bytea,
    -- A scheme without its nonce could not be opened; a nonce without a
    -- scheme would be read as plaintext and hand a garbage seed to the
    -- signer. Neither half is meaningful alone.
    ADD CONSTRAINT ticket_keys_encryption_complete CHECK (
        (encryption IS NULL AND nonce IS NULL)
        OR (encryption IS NOT NULL AND nonce IS NOT NULL)
    );
