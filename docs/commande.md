
cargo generate-lockfile
cargo fmt --all -- --check
cargo check --locked --all-targets --all-features
cargo test --locked --all-targets --all-features

# 5. Analyse statique stricte (aucun warning autorisé)
cargo clippy --locked --all-targets --all-features -- -D warnings

# 1. Générer le fichier de verrouillage pour le nouveau workspace
2. # 2. Vérification du formatage (ne doit rien modifier, juste vérifier)
3. # 3. Vérification de la compilation (avec le lockfile et toutes les fonctionnalités)
# 4. Exécution des tests (doit passer, même s'il n'y a que des tests vides pour l'instant)
