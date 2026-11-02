-- Semantic review rules: approvals needed when a review only changes docs or
-- comments/formatting (NULL = same as required_approvals).
ALTER TABLE protected_refs ADD COLUMN trivial_approvals INTEGER;
