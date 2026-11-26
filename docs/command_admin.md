Rotation sans interruption de service
1. Créer une seconde clé pour le même ledger et le même service.
2. Déposer cette nouvelle clé dans le gestionnaire de secrets et redéployer le service.
3. Vérifier que les appels avec la nouvelle clé fonctionnent.
4. Révoquer l'ancienne clé, puis vérifier qu'elle renvoie 401.

#Créer une clé de paiement
LEDGER_ACTOR=platform-operator-01 \
kauri-cli keys create \
  --ledger "LEDGER_UUID" \
  --role write \
  --label payments-api \
  --environment live \
  --expires never

#Lister les clés
kauri-cli keys list --ledger "LEDGER_UUID"

#Révoquer une clé
LEDGER_ACTOR=platform-operator-01 \
kauri-cli keys revoke --id "KEY_UUID"