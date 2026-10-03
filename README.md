<div align="center">

![Codenotch](docs/design/codenotch-banner.png)

# Codenotch pour Fedora Linux

[![Fedora CI](https://github.com/coroller1972/codenotch-fedora/actions/workflows/fedora.yml/badge.svg)](https://github.com/coroller1972/codenotch-fedora/actions/workflows/fedora.yml)
![Plateforme](https://img.shields.io/badge/Fedora-44%20%7C%20x86__64-51A2DA?logo=fedora&logoColor=white)
![Rust et Tauri](https://img.shields.io/badge/Rust-Tauri%202-orange)
[![Licence MIT](https://img.shields.io/badge/licence-MIT-green)](LICENSE)

**Vos quotas d’assistants de programmation et leur activité, dans une encoche au bord de l’écran.**

[![Télécharger le RPM Fedora](https://img.shields.io/badge/Télécharger-RPM%20Fedora%20x86__64-51A2DA?style=for-the-badge&logo=fedora&logoColor=white)](https://github.com/coroller1972/codenotch-fedora/releases/latest/download/Codenotch-fedora-x86_64.rpm)

</div>

Ce dépôt, **[coroller1972/codenotch-fedora](https://github.com/coroller1972/codenotch-fedora)**,
est un fork Linux de **[vinzdg/codenotch](https://github.com/vinzdg/codenotch)**,
créé par Vinz. Le portage Fedora adapte la base Rust/Tauri du dossier
[`windows/`](windows/) avec GTK 3, WebKitGTK et XWayland.

Ce fork distribue des **RPM pour Fedora 44 x86_64**. Les versions macOS et
Windows restent disponibles dans le [projet d’origine](https://github.com/vinzdg/codenotch).

## Télécharger et installer

Télécharger [le RPM de la dernière release stable](https://github.com/coroller1972/codenotch-fedora/releases/latest/download/Codenotch-fedora-x86_64.rpm),
puis, depuis le dossier de téléchargement :

```sh
sudo dnf install ./Codenotch-fedora-x86_64.rpm
```

DNF installe les dépendances. Lancer ensuite **Codenotch** depuis le menu des
applications, ou avec `codenotch`. Ouvrir à nouveau l’application affiche ses
réglages lorsqu’elle est déjà lancée.

Les [releases du fork](https://github.com/coroller1972/codenotch-fedora/releases)
contiennent le RPM et `SHA256SUMS`. Après avoir téléchargé les deux fichiers dans
le même dossier, vérifier le téléchargement avec `sha256sum -c SHA256SUMS`.
Le RPM n’est pas signé ; la somme de contrôle vérifie son intégrité.

Le lien de téléchargement stable devient disponible après la première release
produite par un tag de version. Pour essayer un commit avant sa publication,
ouvrir un [build Fedora réussi](https://github.com/coroller1972/codenotch-fedora/actions/workflows/fedora.yml)
et télécharger l’artefact **`codenotch-fedora-rpm`**, puis extraire le RPM du ZIP.
GitHub demande une connexion pour télécharger ces artefacts.

Pour mettre à jour, télécharger le nouveau RPM, relancer la commande DNF, puis
redémarrer Codenotch. Les réglages sont conservés. Ce fork ne configure pas de
dépôt DNF et ne télécharge pas les installateurs macOS ou Windows.

## Fonctionnalités Linux

- Encoche sur un bord de l’écran, déploiement au survol et cartes détaillées des quotas.
- Fournisseurs de la base Rust : Claude Code, Codex, Cursor, Grok, GitHub Copilot,
  Antigravity, GLM et OpenCode Go, selon les outils et comptes présents.
- Indicateurs d’activité, thèmes clair et sombre, choix de l’écran et taille de l’encoche.
- Hooks Claude Code, entrée dans le menu des applications et démarrage automatique XDG.
- Compatibilité GNOME/KDE via XWayland, y compris sur une session Wayland.

L’environnement de référence est **Fedora Workstation 44 x86_64**. Sous GNOME,
l’extension **AppIndicator and KStatusNotifierItem Support** permet d’afficher
l’icône de notification ; l’encoche fonctionne sans cette extension.

Le portage ne reprend pas toutes les fonctions de l’application Swift : Phone
Link, Ollama et LM Studio ne sont pas implémentés dans cette version. Le retour
au terminal est limité aux fenêtres X11/XWayland. Consulter le
[guide Fedora](linux/README.md) pour les détails, le dépannage graphique et
l’installation des hooks.

## Compiler depuis les sources

```sh
git clone https://github.com/coroller1972/codenotch-fedora.git
cd codenotch-fedora
make deps
make build
make run
```

Pour une installation utilisateur sans RPM : `make install`. Elle place les
exécutables dans `~/.local/lib/codenotch/` et ajoute l’entrée au menu. Choisir
l’installation utilisateur **ou** le RPM pour éviter deux copies de l’application.

```sh
make test
make rpm
# RPM : windows/target/release/bundle/rpm/*.rpm
```

La configuration et les journaux sont dans `~/.config/codenotch/`, ou dans
`$XDG_CONFIG_HOME/codenotch` si cette variable est définie.

## CI et publication des RPM

Le workflow [Fedora Linux](.github/workflows/fedora.yml) s’exécute sur les pushs
vers `main`, les pull requests, les tags `v*` et les lancements manuels. Il compile
sur Fedora 44, exécute les tests Rust, JavaScript et Python ainsi que les tests
graphiques sous Xvfb, puis construit le RPM avec la CLI Tauri 2.11.4.

Chaque build réussi fournit `Codenotch-fedora-x86_64.rpm` et `SHA256SUMS` dans
l’artefact `codenotch-fedora-rpm`. Un **push de tag `v<VERSION>`** publie aussi ces
fichiers dans une release GitHub du fork. Le tag doit correspondre à la version
de `windows/codenotch/tauri.conf.json` et `windows/codenotch/Cargo.toml`. Un tag
contenant un suffixe de préversion publie une prerelease.

Le nom du RPM publié reste fixe pour conserver le lien de téléchargement du
README ; le paquet contient sa version réelle. Aucun secret de signature Apple
ou Windows n’est nécessaire : la publication utilise le `GITHUB_TOKEN` de la CI.
Les workflows macOS et Windows hérités sont réservés au dépôt d’origine.

## Origine et licence

Merci à **Vinz** et aux contributeurs de
[vinzdg/codenotch](https://github.com/vinzdg/codenotch) pour le projet d’origine,
son interface et la base Rust/Tauri. Le code Swift et la documentation historique
restent présents dans ce dépôt pour conserver leur provenance.

Le portage Linux est développé dans
[coroller1972/codenotch-fedora](https://github.com/coroller1972/codenotch-fedora).
Signaler les problèmes Fedora dans les
[issues de ce fork](https://github.com/coroller1972/codenotch-fedora/issues).

[Licence MIT](LICENSE) — copyright d’origine © 2026 Vinz.
