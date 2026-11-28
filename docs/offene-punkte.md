# RawGraph – Fragen und offene Punkte

Stand: 28.11.2026, Version 26.11.3 (nach Welle 8 und dem Auftrag vom 28.11.: Masken-Migration, GPU-Ausbau, Export-Vorschau/WebP-ICC, HEIC, Depth-Worker, Altmängel, LUT, Gruppen und Makros, HDR Merge, Panorama, HDR-Panorama, Stichwörter, Benennung aus der Beschriftungs-Excel, Stapel-Umbenennung, Arbeitsbereiche). Grundlage sind `README.md` (Anforderungen und Fahrplan) und `node_structure.md` (Node-Liste).

Antworten bitte mit der Nummer. Wo ein Vorschlag steht, reicht „Vorschlag“.

**Hinweis:** Wo keine Antwort vorlag, habe ich mit meinem Vorschlag weitergearbeitet. Diese Stellen stehen unter „Umgesetzt nach Vorschlag“ und lassen sich ändern.

## 1. Fragen an dich

### Noch wirklich offen

| Nr. | Frage | Mein Vorschlag |
|---|---|---|
| 3 | Darf ich das Programm selbst mit Maus und Tastatur bedienen? Das übernimmt für einige Minuten deinen Bildschirm und braucht deine Freigabe am Rechner. Ohne das bleibt alles Interaktive ungeprüft (siehe Abschnitt 3). | Ja, zu einem Zeitpunkt, den du nennst. |
| 7 | Die Ansicht nutzt eine Näherung der ACES-Wiedergabe. Soll die echte ACES-Ausgabe ACES 1.3 oder 2.0 werden? | ACES 2.0. |
| 18 | Welche CPU hat der Hauptrechner (RTX 5090, 128 GB)? | – |
| 19 | Hat einer deiner Monitore HDR, und welches Ziel gilt (PQ oder HLG, wie viele Nits)? Ohne Antwort baue ich kein HDR. | – |
| 21 | macOS und Linux stehen jetzt als Pflicht in den Anforderungen. Ich kann hier nur Windows prüfen. Auf welchem Gerät lässt sich das prüfen, und bis wann? | Sobald ein Mac oder Linux-Rechner mit Rust bereitsteht. |
| 22 | Lizenz: Die RAW-Bibliothek `rawler` steht unter LGPL-2.1 und ist fest in die exe gelinkt. Bei Weitergabe entstehen Pflichten; das Paket beantwortet sie nur mit einem Quelltext-Angebot. Bleiben wir dabei? | Vorerst ja; vor einem Verkauf neu entscheiden. |
| 23 | Die Standard-Helligkeit für RAW ist an deinen zwölf Beispielen gemessen (+1,6 Blenden, dazu der Wert aus der Datei). Canon liegt rund 0,2 Blenden über Sony. Soll es einen Wert je Kamera geben? | Ja, sobald mehr Beispiele je Kamera vorliegen. |
| 27 | Lens Correction „Auto“ nutzt die Korrekturdaten in der Datei. Die gibt es nur bei Sony-DNG (Farbsäume, minimale Verzeichnung, keine Vignette). Canon-DNG und ARW haben keine. Soll eine Objektivdatenbank dazu (Lensfun: Programmteil LGPL-3, Daten CC-BY-SA)? | Ja, als eigene Welle. |
| 28 | Layer-Modus „All“ (jede Kombination): Wie sollen die Ergebnisse heißen und im Filmstreifen erscheinen? | Jede Kombination als eigener Eintrag „A × B“, Export-Name aus beiden Dateinamen. |
| 29 | Zusammenarbeit: Projektdatei im geteilten Ordner mit Revisionsprüfung (jetzt gebaut) oder später ein Datenbank-Server? Die Abwägung steht in `docs/project-format.md`. | Vorerst geteilter Ordner; Server erst bei echtem Mehrbenutzer-Bedarf. |

### Umgesetzt nach Vorschlag (bitte widersprechen, falls falsch)

| Nr. | Thema | So ist es jetzt |
|---|---|---|
| 1 | Commit | Lokale Commits, kein Push. In Git war kein Autor hinterlegt; Name und Mailadresse wurden nur für diese Commits angegeben. |
| 2 | UI-Technik | Natives Rust-Fenster (egui) bleibt. |
| 4 | Baseline-Belichtung | Develop-Node wendet sie automatisch auf RAW an, abschaltbar. |
| 5 | Belichtungswert | +1 macht das Bild eine Blende heller. |
| 6 | Weißabgleich | Absolut in Kelvin und Tönung (höhere Tönung = mehr Magenta, wie Lightroom), relative Verschiebung in Mired. |
| 8 | Ausgebrannte Lichter | Werden neutral gemischt, abschaltbar; keine Rekonstruktion. |
| 9 | Split-Node | Sechs benannte Bereiche plus „Unmatched“. |
| 10 | Bilder ohne Messwert | Beim Filter standardmäßig ausgeschlossen, beim Split-Node standardmäßig „Unmatched“; je Node einstellbar. |
| 11 | Dasselbe Bild über mehrere Merge-Eingänge | Erster verbundener Eingang gewinnt. |
| 13 | Rauschminderung | Klassisch, auf ISO 2500 abgestimmt, wahlweise an die ISO gekoppelt. |
| 16 | XMP | Sterne, „abgelehnt“ und Farbetiketten werden gelesen; geschrieben wird nichts. |
| 20 | Scope-Zeitziel auf CPU | Zurückgestellt. |
| 24 | GPU-Rechenweg im Fenster | Eingebaut, mit automatischem Rückfall auf die CPU. |
| 25 | Export-Farbraum | sRGB, Display P3, Adobe RGB und Rec. 2020 mit eingebettetem Profil. Mit der Wiedergabe „ACES (approx.)“ bleiben die Farben im sRGB-Umfang; die größeren Farbräume wirken erst mit „Clip“ oder „Soft highlights“ voll. |
| 30 | Merge bei gleichem Dateinamen | Standard „beide behalten“; wählbar „erster Eingang gewinnt“ oder „letzter Eingang gewinnt“. |
| 31 | Layer-Paarung „Match“ | Die Ebene mit den meisten Bildern bestimmt die Sammlung; kürzere Ebenen wiederholen sich (10 und 5 Bilder: 1–5 mit 1–5, 6–10 wieder mit 1–5). Ebene 1 liegt unten und bestimmt die Bildgröße. |
| 32 | Export-Node bei vorhandener Datei | Standard „Skip“. Zwei gleichnamige Bilder in einem Lauf: bei „Rename“ durchnummeriert, bei „Skip“ das zweite übersprungen, bei „Overwrite“ Abbruch vor dem Schreiben. |
| 33 | Export in Quellordner | Wird abgelehnt: In einen Ordner mit Quellfotos schreibt das Programm nie. |
| 34 | Projektformat | Format 2 mit fester Projekt- und Bild-ID, Revision je Speichern, Verlauf mit Graph-Ständen (höchstens 100 Einträge und 32 MB). Alte Dateien werden beim Öffnen umgewandelt; vor dem ersten Überschreiben entsteht eine Kopie `….v1.bak`. |
| 35 | Konflikt beim Speichern | Hat jemand anderes inzwischen gespeichert, fragt das Programm: als Kopie speichern, überschreiben (mit zweiter Rückfrage) oder abbrechen. Zusammengeführt wird nichts. |

Offen geblieben und nicht begonnen: Frage 15 (Benennung aus deiner Beschriftungs-Excel) und Frage 17 (Ort der Projektdatei; bisher dort, wo du speicherst).

## 2. Bekannte Mängel

- **Layer nur im Modus „Match“.** „All combinations“ ist sichtbar, aber ausgegraut.
- **Lens Correction „Auto“** wirkt nur bei Sony-DNG. Ob die Korrektur genau so sitzt wie bei Adobe, ist nicht gegengeprüft (Annahme zum Bezugsrahmen der Daten).
- **AVIF ist langsam:** 3–5 s je Bild bei Langkante 2048, rund 7,5 s bei 24 Megapixeln. Adobe RGB geht als AVIF nicht. Die AVIF-Pixel sind nicht durch Zurücklesen geprüft, nur der Dateiaufbau.
- **Exportierte Farbprofile** sind durch Tests belegt, aber in keinem farbverwalteten Betrachter (Photoshop) angesehen.
- **GPU-Vorschau:** Develop-Regler, Belichtung, Kontrast, Kurven (alle sieben Arten), Farbton/Sättigung, Farbräder, Vignette und die Anzeige laufen auf der GPU, ebenso die Nachbarschafts-Operationen Develop Lichter/Schatten/Weiß/Schwarz (lokale Tonwerte; das 512-px-Gitter wird auf der CPU geschätzt), Filter (Gauß und Box, Tief- und Hochpass) und Schärfen. Layer, Lens Correction, Rauschminderung, Klarheit, Zuschneiden, Filter mit Radius-Karte oder anderem Kern, Schärfen mit Mengen-Karte, alles mit Maske, die Vergleichsansicht und der Export laufen auf der CPU (siehe README, „What runs on the GPU“).
- **Bildwechsel** wird durch das Vorladen der Nachbarbilder nur um 7–11 ms schneller.
- **Pinsel reagiert träge:** rund 50 ms von der Mausbewegung bis zum Bild bei 30 Strichen; im Fenster nicht nachgemessen.
- **Speichern während eines Ordner-Scans** geht jetzt: gespeichert wird, was bis dahin gefunden ist; der Scan läuft weiter und ergänzt danach ohne falschen Konflikt (Revision und Verlauf werden übernommen). Noch nicht fertig gehashte Bilder stehen als „ausstehend“ in der Datei und werden beim nächsten Scan nachgeholt.
- **Speichern schreibt nur Geändertes** (nur auf lokaler fester Platte außerhalb von Sync-Ordnern; auf Netzlaufwerken und in Nextcloud/Dropbox/OneDrive-Ordnern sowie mit der Einstellung „Always rewrite“ wird die ganze Datei neu geschrieben, siehe `docs/project-format.md`): eine vorhandene Projektdatei (gleiches Projekt, aktuelles Schema) wird in einer Transaktion im Rollback-Journal an Ort und Stelle aktualisiert (geänderte Bildzeilen, Graph, neuer Verlaufseintrag). Die Datei wird dabei gelesen (Bildzeilen und Graph zum Vergleich), das kostet auf dem NAS einige MB Leseverkehr. Gelöschte Verlaufseinträge verkleinern die Datei nicht; „Speichern unter“ schreibt sie neu und kompakt.
- **Statuszeile:** Der Text „autosave“ wird bei langen Meldungen rechts überdeckt.
- **Schärfen in der verkleinerten Vorschau kaum sichtbar;** beurteilen lässt es sich nur bei 100 %.
- **Weichzeichner mit großem Radius** rechnet auf einem verkleinerten Bild; Abweichung 1–2 %, vor allem am Rand.
- **Rückgängig-Speicher** teilt unveränderte Knoten zwischen den bis zu 200 Schritten (Arc, Kopie beim Schreiben). Wird derselbe Knoten in jedem Schritt geändert (z. B. Strich für Strich in einem Draw-Knoten mit Tausenden Strichen), hält jeder Schritt weiterhin eine Kopie dieses einen Knotens; Teilen auf Strichebene wäre der nächste Schritt.
- **Erster Scan auf dem NAS:** rund 35 ms je Datei, also etwa zwei Minuten für 3.700 Bilder.
- **Bildwahl bis Bild auf dem NAS** (gemessen 28.11.2026, Release, `--bench-first-frame`, 33 DNG à 31 MB, Vorschau 2560, zwei Runden): erstes Bild (Vorschaubild/eingebettete Vorschau) im Median 43 ms (p95 59 ms) ungecacht, 10 ms gecacht; fertig gerendertes Bild 0,66 s (p95 0,71 s) ungecacht, 0,57 s gecacht. Die Endbilder liefen dabei über die CPU (GPU durch einen anderen Job zu 6,3 von 8 GB belegt). Der Ordner blieb unverändert.
- **XMP-Import** liest Begleitdateien mit einem eigenen Pool aus 8 Threads (nicht mehr auf dem Render-Pool); Ergebnis und Reihenfolge sind unabhängig vom Tempo der einzelnen Dateien.
- **Arbeitsspeicher beim Export:** 1,4 GB je Bild, mit Rauschminderung in voller Größe 3,1 GB; ein Layer-Bild braucht das je Ebene.
- **Vorschaubilder** beachten Farbprofile der Eingabedatei nicht.
- **Weitergabe-Paket:** nicht signiert (Windows warnt beim Start), kein Installer, auf keinem zweiten Rechner gestartet.

## 3. Nicht von Hand geprüft

Für diese Punkte gibt es automatische Tests mit künstlichen Eingaben und Bildschirmfotos; niemand hat sie mit echter Maus und Tastatur bedient:

- Node-Editor: Verbinden, Ziehen, Mehrfachauswahl, Gummiband, Kopieren/Einfügen, Rechtsklick-Menü, Tab-Suche
- Regler ziehen, Strg+Z und Strg+Y
- Kurven-Editor mit seinen sieben Reitern, Pinsel, Zuschnitt-Anfasser
- Alle Dateidialoge (Ordner öffnen, Ordner im Source- und Export-Node wählen, Speichern, Exportieren, Datei suchen, Rezept importieren)
- Textfelder im Export-Node
- Verlaufsfenster („Restore this graph“), Konfliktabfrage, Rückfrage bei ungespeicherten Änderungen
- Export-Dialog im Node-Modus, „Run Export nodes…“, Abbrechen, „Retry failed“
- Bewertungstasten 0–5, P, X, U, 6–9; Taste C für den Vergleich
- Rezepte umbenennen und löschen, „Use anyway“ beim Neuverknüpfen, „Clear thumbnail cache“, „Discard“ im Wiederherstellungs-Dialog
- Clean Feed, Vollbild, zweites Fenster, Verhalten beim Abziehen eines Monitors
- Scan eines großen NAS-Ordners im Fenster
- Lauf auf einem anderen Rechner

## 4. Noch nicht gebaut

Reihenfolge nach meinem Vorschlag.

1. **Layer-Modus „All“** und zusammengesetzte Bild-Identitäten (Frage 28).
2. **ACES-Ausgabe und HDR:** echte Output-Transforms, HDR-Vorschau, HDR-Export, Scopes mit Nits-Skala (Fragen 7 und 19).
3. **Objektivdatenbank** für Verzeichnung und Vignette (Frage 27); Farbsäume aus dem Bild schätzen; Kameraprofile. Für Panoramen mit starkem Weitwinkel wichtig (Verzeichnung wird beim Zusammensetzen nicht korrigiert).
4. **GPU-Rechenweg ausbauen:** Masken, Layer (Platzieren und Mischen), Rauschminderung (Guided Filter), Lens Correction, Geometrie, Scopes auf der GPU; Filter mit Radius-Karte. Develop Lichter/Schatten/Weiß/Schwarz, Filter (Gauß/Box) und Schärfen laufen bereits dort.
5. **Lokale Werkzeuge:** Dunst entfernen, Fleckenentfernung, Lichter/Schatten mit lokaler Anpassung.
6. **Graph-Bedienung:** Kommentare, Ausrichten. (Gruppen und Makros sind gebaut.)
7. **Weitere Nodes:** Körnung; Masken aus KI (Motiv, Himmel), importiertes Bild. (LUT ist gebaut.)
8. **Mehrbild-Funktionen:** Fokus-Stapel; HDR-Ausrichtung mit Drehung/Subpixel; Gruppen von Hand per Auswahl im Filmstreifen bilden (heute: „Group number“ je Bild). HDR Merge, Panorama und HDR-Panorama sind gebaut, aber nur mit Prüfbildern getestet, nicht mit echten Reihen von Hand.
9. **Bibliothek:** Stapel, Analyse-Filter (Helligkeit); Stichwörter in HEIC-Dateien. (Stichwörter, XMP schreiben, Benennung aus der Beschriftungs-Excel und Stapel-Umbenennung sind gebaut.)
10. **Zusammenarbeit:** Zusammenführen gleichzeitiger Änderungen, Sperren, Rechte, geteilter Speicher (Frage 29).
11. **Scopes:** HDR-Skalen, CIE-Farbtafel, Vergleich mit Resolve an Testbildern.
12. **Arbeitsbereiche:** Scopes und Clean Feed auf drittem und viertem Monitor. (Gespeicherte Layouts für mehrere Monitore sind gebaut.)
13. **Betrieb:** Installer, Signatur, Handbuch, macOS und Linux (HEIC dort ohne Encoder).

## 5. Was steht

- Zwölf Pakete (dazu `rawgraph-rename`), rund 1.800 Tests grün (ohne die GPU-Tests auf echter Grafikkarte), Clippy ohne Warnungen. Quelltext auf GitHub (`CuckCodingClan/rawgraph`, privat); jede Änderung als Pull Request mit Prüfung unter Linux und Windows.
- GPU-Vorschau: CPU- und GPU-Bild weichen um höchstens eine von 255 Stufen ab; bei jedem GPU-Fehler fällt die Vorschau auf die CPU zurück.
- Geladen und geprüft: Sony A7 IV (ARW, DNG), Canon EOS R5 und R6 (DNG), JPEG, PNG, TIFF. Bildwahl bis Bild vom NAS: erstes Bild 43 ms, fertiges Bild 0,66 s (Median, 33 DNG).
- 34 Nodes, neu seit 15.11.: LUT (`.cube`), HDR Merge, Panorama, Depth Map mit eigenem Arbeitsprozess; dazu Gruppen (Strg+G) und Makros.
- Graph-Format 6 (Masken-Migration aus Format 1/2, Develop-Ton, Stichwörter, Gruppen); ältere Dateien werden beim Laden umgeschrieben, neuere klar abgelehnt. Projektdatei Schema 3 mit Verlauf, Speichern während eines Scans, Wiederherstellung nach Absturz; beschrieben in `docs/project-format.md`.
- Export: JPEG, PNG, TIFF (8 und 16 Bit), EXR, AVIF, WebP (mit ICC), HEIC (Windows-Encoder); Stichwörter als XMP eingebettet und als Begleitdatei; Vorschau des Codecs im Viewer.
- Bibliothek: Stichwörter (Panel, Filter, Lightroom-XMP), Benennung aus der Beschriftungs-Excel (`{sheet_name}`, `{caption}`, `{no}`), Stapel-Umbenennung auch der RAW-Originale mit Vorschau, Blockprüfung, Rückgängig-Protokoll und Wiederherstellung nach Abbruch.
- Arbeitsbereiche für mehrere Monitore (Primary, Dual Screen, Scopes, Clean Feed), gespeichert und wiederherstellbar.
- Weitergabe-Paket: `packaging\package.ps1` baut Ordner und zip nach `%LOCALAPPDATA%\rawgraph\dist\`.
