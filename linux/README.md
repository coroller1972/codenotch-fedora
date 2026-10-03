# Codenotch sur Fedora Linux

Ce portage est développé dans le fork
[coroller1972/codenotch-fedora](https://github.com/coroller1972/codenotch-fedora),
à partir de [vinzdg/codenotch](https://github.com/vinzdg/codenotch).
Pour installer le RPM construit par la CI, consulter
[les téléchargements et instructions du README principal](../README.md#télécharger-et-installer).

Le portage Fedora utilise l'application Rust/Tauri du dossier `windows/` :
interface GTK 3 + WebKitGTK 4.1, encoche transparente, suivi des quotas et des
sessions. Les sources sont partagées avec Windows ; l'application Swift macOS
reste indépendante. L'environnement de référence est Fedora Workstation 44.

## Compiler et lancer

Depuis la racine du dépôt :

```sh
make deps       # paquets Fedora, demande sudo
make build      # compile codenotch ET codenotch-hook en release
make run
```

Le premier build télécharge les dépendances Rust et peut prendre plusieurs
minutes. Les exécutables sont dans `windows/target/release/`.
Le lancement direct fonctionne aussi :

```sh
./windows/target/release/codenotch
./windows/target/release/codenotch doctor
```

`doctor` fonctionne sans écran. Il indique les chemins de configuration et les
sources détectées ; son rapport peut contenir des chemins et identifiants de
sessions, à relire avant de le partager.

## Installer pour votre compte

```sh
make install
```

Cette installation ne demande pas sudo. Elle ajoute Codenotch au menu des
applications, copie les deux exécutables dans `~/.local/lib/codenotch/` et crée
les liens `~/.local/bin/codenotch` et `~/.local/bin/codenotch-hook`.
La configuration et les journaux sont dans `$XDG_CONFIG_HOME/codenotch`
(`~/.config/codenotch` par défaut). Le lanceur et l'icône respectent
`XDG_DATA_HOME`. Les icônes sont installées en 32, 128, 256 et 512 pixels ; le
lanceur indique aussi la classe de fenêtre pour l'association sous GNOME.
Relancer l'application ouvre les réglages si elle tourne déjà.

Le démarrage à la connexion se règle dans **General → Open Codenotch at login**,
ou depuis le terminal :

```sh
~/.local/bin/codenotch autostart on
~/.local/bin/codenotch autostart off
```

Pour activer les événements Claude Code, utiliser **Install hooks** dans les
réglages, ou :

```sh
~/.local/bin/codenotch install-hooks
```

Le fichier `~/.claude/settings.json` est sauvegardé avant modification. Les hooks
des autres outils sont conservés. Le helper lit le même port XDG que
l'application et peut la démarrer lorsqu'un événement arrive.

Pour mettre à jour, récupérer les sources puis relancer `make install` et
redémarrer Codenotch. La version Linux ne propose pas d'installateurs Windows.
Pour désinstaller, quitter l'application puis :

```sh
~/.local/bin/codenotch autostart off
~/.local/bin/codenotch uninstall-hooks
make uninstall
```

Les réglages, historiques et identifiants des fournisseurs sont conservés.

## GNOME, KDE et Wayland

L'encoche s'exécute avec **XWayland**, même sur une session Wayland. Ce choix
permet le positionnement précis sur un bord de l'écran, le suivi du pointeur et
le déplacement entre les bords. Il est appliqué dans le programme, donc reste
valable depuis le menu, le démarrage automatique et les hooks.
Le serveur `xorg-x11-server-Xwayland` doit être installé et `DISPLAY` disponible.

Le survol utilise une région d'entrée X11 limitée à l'encoche, à sa bande de
réveil et à sa carte ouverte. Elle reste active quand le pointeur se trouve
sur une application Wayland ; le reste de la fenêtre transparente laisse
passer les clics. L'encoche se replie lorsque la souris quitte cette région.
L'entrée et la sortie sont suivies directement par X11, même si WebKit perd
l'événement de sortie pendant une animation. Les animations d'activité sont
retirées au repli et rétablies au prochain survol avec l'état actuel.

WebKitGTK transporte par défaut les images en mémoire partagée, pour éviter
`Failed to create GBM buffer … Invalid argument` avec certains pilotes sous
XWayland tout en conservant son moteur de composition. Désactiver entièrement
ce moteur avec l'ancien réglage `WEBKIT_DISABLE_DMABUF_RENDERER=1` peut faire
disparaître les cartes et clignoter les animations sur WebKitGTK 2.54 ; retirer
cette variable si elle a été ajoutée à votre environnement. Le nouveau réglage
s'applique aussi aux lancements directs, au menu et aux hooks. Pour utiliser
les buffers matériels sur un pilote compatible :

```sh
WEBKIT_DMABUF_RENDERER_FORCE_SHM=0 make run
```

Le réglage ne modifie pas le pilote ni la configuration graphique du système.
Voir le [choix du transport dans WebKit](https://github.com/WebKit/WebKit/blob/main/Source/WebKit/UIProcess/gtk/AcceleratedBackingStore.cpp).

Sur GNOME, l'icône de la zone de notification nécessite l'extension
**AppIndicator and KStatusNotifierItem Support**. Elle est facultative pour
l'encoche et les réglages : rouvrir Codenotch depuis le menu permet d'accéder
aux réglages. KDE propose généralement cette zone directement.

La connexion Claude utilise Ptyxis sur Fedora, ou un terminal compatible déjà
installé. Elle ne lance une connexion que sur demande explicite depuis la carte.

## Fonctions et limites

- Encoche, cartes de quotas, réglages, thèmes, choix de l'écran et du bord,
  déplacement, hooks Claude et démarrage XDG : adaptés à Linux.
- Fournisseurs de la base Rust : Claude, Codex, Cursor, Grok, Copilot, GLM,
  OpenCode Go et Antigravity. Ils utilisent les sessions déjà présentes.
  La découverte Copilot utilise `gh`, et la récupération Codex peut lancer un
  exécutable ELF natif, y compris celui d'une installation npm.
- Antigravity : le CLI officiel `agy` est cherché dans `~/.local/bin` et `PATH` ;
  sa commande de quotas est limitée dans le temps. Sans CLI, le pont d'une IDE
  déjà ouverte peut être utilisé si `lsof` est installé. La récupération directe
  d'identifiants depuis le coffre Windows n'existe pas sous Linux.
- Retour au terminal et acquittement d'une session vue : disponibles pour les
  fenêtres X11/XWayland. Les fenêtres Wayland natives ne sont pas accessibles
  par ce mécanisme ; elles doivent être activées depuis le bureau.
- La couleur automatique selon le contenu derrière l'encoche est masquée sous
  Linux : la capture Windows ne peut pas lire le bureau Wayland.
- La base Rust ne possède pas tous les fournisseurs et fonctions spécifiques à
  l'application Swift (par exemple les intégrations locales Ollama/LM Studio et
  Phone Link). Ce portage ne les réimplémente pas.
- Les essais automatiques utilisent des données synthétiques ; ils ne
  garantissent pas l'accès à chaque API pour votre abonnement.

## Tests et RPM

```sh
make test
python3 linux/verify_runtime.py  # après make build, fixtures XDG temporaires
python3 linux/verify_hover.py    # Xvfb : survol maintenu, pixels natifs, repli et clics traversants
# Rendu WebKit et pixels après repli (python3-gobject et python3-cairo) :
xvfb-run -a dbus-run-session -- python3 linux/verify_render.py
make rpm
# → windows/target/release/bundle/rpm/*.rpm
```

`make rpm` utilise la CLI Tauri 2.11.4 via npm et inclut `codenotch-hook` dans
`/usr/bin`, à côté de l'application. Pour installer le paquet produit :

```sh
sudo dnf install ./windows/target/release/bundle/rpm/*.rpm
```

Le RPM n'est pas signé. Choisir l'installation utilisateur **ou** le RPM
pour éviter deux copies dans le menu. La CI `.github/workflows/fedora.yml`
compile, teste et construit le RPM dans un environnement Fedora 44. Chaque build
réussi fournit l'artefact `codenotch-fedora-rpm` avec le paquet renommé
`Codenotch-fedora-x86_64.rpm` et `SHA256SUMS`. Les pushs de tags `v<VERSION>`
publient ces fichiers dans les
[releases du fork](https://github.com/coroller1972/codenotch-fedora/releases) ;
la version du tag doit correspondre à celle de Tauri et du crate `codenotch`.

Un contrôle graphique de huit secondes avec une activité Codex fictive, sans
requêtes de quotas, renouvellement CLI, surveillance de sessions ou serveur
de hooks, est disponible :

```sh
./windows/target/release/codenotch --smoke-test
```

Il ouvre l'encoche et les réglages, puis ferme l'application. Quitter une
instance existante avant de l'exécuter. En CI, utiliser Xvfb et des répertoires
XDG temporaires, comme dans le workflow Fedora.

Références : [prérequis Tauri pour Fedora](https://v2.tauri.app/start/prerequisites/),
[format des entrées desktop](https://specifications.freedesktop.org/desktop-entry/latest/),
[installation du CLI Antigravity](https://www.antigravity.google/docs/cli/install/).
