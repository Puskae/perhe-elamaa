-- Admin-hidden bottom-bar features, family-wide. One row per HIDDEN feature;
-- absence means shown, so a new family (and every existing one) starts with
-- everything visible. `feature` is perkele_shared::feature::Feature::key();
-- unknown keys (e.g. written by a newer version) are skipped on read.
-- Hiding keeps the feature's data; it only hides the tab and mutes its pushes.
CREATE TABLE family_hidden_features (
    family_id INTEGER NOT NULL REFERENCES families (id),
    feature   TEXT    NOT NULL,
    PRIMARY KEY (family_id, feature)
);
