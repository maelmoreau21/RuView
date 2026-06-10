# Workflows beta et recherche RuvSense

> [!IMPORTANT]
> Ces workflows sont des outils de recherche expérimentaux, désactivés par défaut dans le chemin de production. Ils ne sont pas des dispositifs médicaux, des systèmes de diagnostic, des services d'urgence, ni des substituts à un avis professionnel.

Le README principal garde volontairement une liste de fonctionnalités stable et conservatrice. Ce répertoire contient les workflows beta opt-in dont les sorties doivent être validées localement avant tout usage opérationnel.

| Feature | Statut | Fiabilité honnête |
|---------|--------|-------------------|
| Skeleton 17 keypoints / pose | Beta/expérimental, désactivé par défaut | Pose CSI de recherche; utile pour études contrôlées et données appairées, pas pour du suivi humain général en production. |
| Arrêt cardiaque | Beta/expérimental, désactivé par défaut | Non validé cliniquement et pas un détecteur d'urgence. A traiter comme signal exploratoire seulement. |
| Fréquence cardiaque précise | Beta/expérimental, désactivé par défaut | Tendance/screening seulement; mouvement, respiration et multipath peuvent dominer le signal. |
| Comptage multi-personnes précis | Beta/expérimental, désactivé par défaut | L'occupation approximative peut fonctionner; le comptage précis nécessite géométrie contrôlée, calibration et validation locale. |
| DensePose / reconstruction 3D | Beta/expérimental, désactivé par défaut | Surface de démo/recherche pour benchmarks et visualisation, pas une reconstruction production-grade. |
| WASM cardiac-arrhythmia | Beta/expérimental, désactivé par défaut | Prototype de screening d'arythmie; pas un diagnostic et pas activé par le script stable. |

## Script de monitoring santé

`beta/setup_health_monitoring.py` est le bootstrap complet déplacé, susceptible d'activer les modules runtime cardiaques et arythmie comme `cardiac_arrhythmia` ainsi que des modules beta de tendances. Utilisez-le uniquement pour des exécutions explicites de recherche/test:

```bash
python -m pip install requests
python beta/setup_health_monitoring.py
```

Le script stable reste dans `scripts/setup_health_monitoring.py` et évite volontairement les modules runtime cardiaques.

## Contenu du dossier

Ce dossier conserve les artefacts experimentaux, references DensePose, ADRs beta, bundles de modeles de recherche et le bootstrap complet de monitoring qui peut activer des modules cardiaques. Les fichiers restent disponibles pour la recherche et les regressions, mais ils sont separes du chemin de production stable.

Certains modules beta doivent rester dans leur chemin de crate d'origine quand Rust ou Python dependent de cette organisation. Dans ce cas, la surface runtime est desactivee par feature flag plutot que deplacer physiquement le fichier au risque de casser la compilation.

## Activation

Les fonctionnalites beta sont `false` par defaut dans `config/features.toml`. Activez-les uniquement pour des sessions explicites de recherche ou de test:

```toml
[beta]
skeleton_pose_estimation = true
cardiac_arrest_detection = true
precise_heart_rate = true
precise_person_counting = true
densepose_3d = true
```

Pour Docker ou des tests temporaires, utilisez l'override d'environnement:

```bash
RUVSENSE_BETA_FEATURES=skeleton_pose_estimation,precise_heart_rate,densepose_3d
```

Quand une API beta est desactivee, le serveur retourne:

```json
{"error":"feature_disabled","feature":"skeleton_pose_estimation","reason":"beta"}
```

## Risques

Les chemins arret cardiaque, arythmie et frequence cardiaque precise ne sont pas valides cliniquement. Les faux positifs et faux negatifs sont dangereux dans un contexte medical ou d'urgence; ces fonctions ne doivent pas servir a diagnostiquer, declencher une intervention, traiter un patient ou prendre une decision de securite vitale.
