# RawGraph – Fragen und offene Punkte

Stand: 15.11.2026, nach Welle 8 (Nodes aus `node_structure.md`, eingefärbte Scopes, Projektformat 2). Grundlage sind `README.md` (Anforderungen und Fahrplan) und `node_structure.md` (Node-Liste).

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
| 26 | HEIC steht in der Node-Liste. Dafür gibt es keinen Encoder mit verträglicher Lizenz (HEVC ist patentbelastet, die freien Encoder stehen unter GPL). Möglich wäre der Windows-eigene Encoder, der nur unter Windows und nur mit installierter HEVC-Erweiterung läuft. Reicht AVIF? | AVIF reicht; HEIC weglassen. |
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
- **GPU-Vorschau:** Develop-Regler, Belichtung, Kontrast, Kurven (alle sieben Arten), Farbton/Sättigung, Farbräder, Vignette und die Anzeige laufen auf der GPU. Layer, Lens Correction, Weichzeichner, Rauschminderung, Schärfen, Klarheit, Zuschneiden, alles mit Maske, die Vergleichsansicht und der Export laufen auf der CPU.
- **Bildwechsel** wird durch das Vorladen der Nachbarbilder nur um 7–11 ms schneller.
- **Pinsel reagiert träge:** rund 50 ms von der Mausbewegung bis zum Bild bei 30 Strichen; im Fenster nicht nachgemessen.
- **Speichern während eines Ordner-Scans** wird abgelehnt („erst nach dem Scan“). Bei einem langen NAS-Scan heißt das warten.
- **Jedes Speichern schreibt die ganze Projektdatei neu,** samt Verlauf bis 32 MB.
- **Statuszeile:** Der Text „autosave“ wird bei langen Meldungen rechts überdeckt.
- **Schärfen in der verkleinerten Vorschau kaum sichtbar;** beurteilen lässt es sich nur bei 100 %.
- **Weichzeichner mit großem Radius** rechnet auf einem verkleinerten Bild; Abweichung 1–2 %, vor allem am Rand.
- **Rückgängig-Speicher** hält bis zu 200 vollständige Graph-Kopien; mit Tausenden Pinselstrichen wird das groß.
- **Erster Scan auf dem NAS:** rund 35 ms je Datei, also etwa zwei Minuten für 3.700 Bilder.
- **XMP-Import** liest Begleitdateien nacheinander; auf dem NAS bei Tausenden Bildern langsam.
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
3. **Objektivdatenbank** für Verzeichnung und Vignette (Frage 27); Farbsäume aus dem Bild schätzen; Kameraprofile.
4. **GPU-Rechenweg ausbauen:** Masken, Layer, Weichzeichner, Schärfen, Rauschminderung, Geometrie, Scopes auf der GPU.
5. **Lokale Werkzeuge:** Dunst entfernen, Fleckenentfernung, Lichter/Schatten mit lokaler Anpassung.
6. **Graph-Bedienung:** Gruppen und Makros, Kommentare, Ausrichten.
7. **Weitere Nodes:** Körnung, LUT; Masken aus KI (Motiv, Himmel), Tiefe, importiertes Bild.
8. **Mehrbild-Funktionen:** HDR-Zusammenführung, Panorama, Fokus-Stapel.
9. **Bibliothek:** Stichwörter, Stapel, XMP schreiben, Benennung aus der Beschriftungs-Excel, Analyse-Filter (Helligkeit).
10. **Zusammenarbeit:** Zusammenführen gleichzeitiger Änderungen, Sperren, Rechte, geteilter Speicher (Frage 29).
11. **Scopes:** HDR-Skalen, CIE-Farbtafel, Vergleich mit Resolve an Testbildern.
12. **Arbeitsbereiche:** gespeicherte Layouts wie in Resolve, Scopes und Clean Feed auf drittem und viertem Monitor.
13. **Betrieb:** Installer, Signatur, Handbuch, macOS und Linux.

## 5. Was steht

- Elf Pakete, 1.112 Tests grün, Clippy ohne Warnungen, lokale Commits ohne Push.
- GPU-Vorschau: CPU- und GPU-Bild weichen um höchstens eine von 255 Stufen ab (bei unter 0,1 % der Pixel); bei jedem GPU-Fehler fällt die Vorschau auf die CPU zurück. Ein Develop-Regler braucht rund 12 ms bis zum Bild (CPU: rund 45 ms).
- Geladen und geprüft: Sony A7 IV (ARW, DNG), Canon EOS R5 und R6 (DNG), JPEG, PNG, TIFF.
- 33 Nodes. Aus `node_structure.md`: Source (Ordner, Unterordner), Metadata Filter, Merge (Namensregel), Layer (Match), Develop (Exposure, Contrast, Lights, Shadows, White, Black, Weißabgleich, Tint, Dynamic, Saturation), Curves (Lum×Lum, Sat×Sat, Hue×Hue, Hue×Sat, dazu Rot/Grün/Blau), Lens Correction (Manual/Auto, Vignette, Farbsäume), Export (Format, Größe, Farbraum, Metadaten, Zielordner, Kollisionsregel). Dazu wie bisher: Capture EV, Exposure, Contrast, Highlights/Shadows, Tone Curve, White Balance, Saturation, Hue/Saturation, Lift/Gamma/Gain, Black & White, Noise Reduction, Sharpen, Clarity, Blur, Crop/Rotate, Vignette, Split by Metadata, sieben Masken-Nodes, Output.
- Scopes eingefärbt (abschaltbar): Wellenform in den Bildfarben, RGB-Überlagerung additiv, Vektorskop nach Farbton, Histogramm in Kanalfarben.
- Projektdatei Format 2 mit Verlauf, Wiederherstellen eines früheren Graphen und Konfliktabfrage; beschrieben in `docs/project-format.md`.
- Export: JPEG, PNG, TIFF (8 und 16 Bit), EXR, AVIF; vier Farbräume; Metadaten wählbar samt GPS, Autor, Copyright, Beschreibung.
- Weitergabe-Paket: `packaging\package.ps1` baut Ordner und zip (exe ohne Microsoft-C++-Laufzeit, Lizenztexte aller Bausteine) nach `%LOCALAPPDATA%\rawgraph\dist\`.
- Starten: `%LOCALAPPDATA%\rawgraph\dist\RawGraph-0.1.0-windows-x64\RawGraph.exe` (optional mit `--open-folder <Ordner>`).
- Aufgeräumt am 15.11.2026: Bau-Ordner, Bildschirmfotos und Testdaten sind gelöscht. Geblieben sind der Code, die Doku, das Paket in `dist` und die Beispielfotos in `samples`. Der nächste Bau übersetzt alles neu (einige Minuten).
