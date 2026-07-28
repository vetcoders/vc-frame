# Vetcoders release signing: runbook od zera

Ten dokument opisuje utworzenie własnego, trwałego źródła zaufania dla
`vc-frame`, CodeScribe i Pensieve. Nie trzeba kupować „certyfikowanego GPG”.
Wartość klucza wynika z tego, że:

- prywatny klucz główny nigdy nie trafia do CI ani na codzienny komputer;
- każdy produkt ma osobny, rotowalny podklucz podpisujący;
- publiczny fingerprint jest publikowany kilkoma niezależnymi kanałami;
- istnieją kopie zapasowe i certyfikat unieważnienia;
- instalatory sprawdzają przypięty fingerprint, a nie dowolny klucz pobrany
  razem z artefaktem.

GPG i trusted publishing rozwiązują dwa różne problemy. GPG daje użytkownikom
stabilną tożsamość Vetcoders także poza GitHubem. OIDC daje pojedynczemu jobowi
CI krótkotrwałą tożsamość bez stałego tokenu do npm, PyPI, crates.io lub
GitHub attestations. Dla publicznego release używamy obu.

## Docelowy układ

```text
Vetcoders offline primary key (tylko certyfikacja, 5 lat)
├── vc-frame release signing subkey (1 rok)
├── CodeScribe release signing subkey (1 rok)
└── Pensieve release signing subkey (1 rok)
```

Klucz główny jest wspólnym publicznym korzeniem organizacji. Podklucze są
oddzielne, więc wyciek sekretu jednego produktu nie zmusza do porzucenia całej
tożsamości Vetcoders. Loctree może dostarczyć sprawdzony proces i automatyzację,
ale nie kopiujemy jego prywatnego klucza ani nie udajemy, że tożsamość Loctree
jest tożsamością Vetcoders.

## Zanim zaczniesz

Przygotuj:

1. skrzynkę, którą Vetcoders będzie kontrolować przez lata, np.
   `releases@vetcoders.io`;
2. odłączony od sieci komputer lub świeży profil systemowy;
3. dwa oddzielne, szyfrowane nośniki na backup;
4. menedżer haseł na długie, losowe hasło klucza;
5. opcjonalnie dwa tokeny OpenPGP do podpisów wykonywanych przez człowieka.

Nie wykonuj ceremonii w zwykłym katalogu `~/.gnupg`. Osobny katalog ułatwia
udowodnienie, że pracujesz na właściwej tożsamości i nie pomieszałeś jej z
kluczami prywatnymi.

## 1. Utwórz offline primary key

Poniższe polecenia uruchom na szyfrowanym, odłączonym nośniku. Zmień ścieżkę i
adres e-mail na faktycznie kontrolowane przez Vetcoders:

```sh
umask 077
VC_GNUPG_HOME="/Volumes/Vetcoders-Secrets/gpg-master"
VC_RELEASE_UID="Vetcoders Release Signing <releases@vetcoders.io>"

install -d -m 700 "$VC_GNUPG_HOME"
gpg --homedir "$VC_GNUPG_HOME" \
  --quick-generate-key "$VC_RELEASE_UID" ed25519 cert 5y

VC_PRIMARY_FPR="$(
  gpg --homedir "$VC_GNUPG_HOME" --batch --with-colons \
    --list-secret-keys "$VC_RELEASE_UID" |
    awk -F: '$1 == "fpr" { print $10; exit }'
)"
printf 'Vetcoders primary fingerprint: %s\n' "$VC_PRIMARY_FPR"
```

`cert` jest celowe: primary key służy do zatwierdzania i odwoływania podkluczy,
nie do codziennego podpisywania release'ów.

Zapisz fingerprint jako publiczną, czterdziestoznakową wartość. Sam fingerprint
nie jest sekretem.

## 2. Dodaj osobny podklucz dla każdego produktu

Wykonaj polecenie raz dla każdego produktu:

```sh
gpg --homedir "$VC_GNUPG_HOME" \
  --quick-add-key "$VC_PRIMARY_FPR" ed25519 sign 1y
```

Po każdym wykonaniu zapisz najnowszy fingerprint podklucza wraz z nazwą
produktu:

```sh
gpg --homedir "$VC_GNUPG_HOME" --batch --with-colons \
  --list-secret-keys "$VC_PRIMARY_FPR" |
  awk -F: '$1 == "fpr" { print $10 }'
```

Pierwsza wartość to primary fingerprint, następne odpowiadają podkluczom.
Prowadź prosty, podpisany tekstowy rejestr:

```text
primary    ABCD...
vc-frame  1111...  expires YYYY-MM-DD
codescribe 2222... expires YYYY-MM-DD
pensieve  3333...  expires YYYY-MM-DD
```

Roczny termin podkluczy wymusza regularną rotację, ale nie zmienia publicznego
primary fingerprintu przypiętego w instalatorach.

## 3. Zrób recovery kit, zanim cokolwiek przeniesiesz

Na pierwszym szyfrowanym nośniku:

```sh
VC_GPG_BACKUP="/Volumes/Vetcoders-Secrets/recovery"
install -d -m 700 "$VC_GPG_BACKUP"

gpg --homedir "$VC_GNUPG_HOME" --armor \
  --export "$VC_PRIMARY_FPR" \
  >"$VC_GPG_BACKUP/vetcoders-release-public.asc"

gpg --homedir "$VC_GNUPG_HOME" --armor \
  --export-secret-keys "$VC_PRIMARY_FPR" \
  >"$VC_GPG_BACKUP/vetcoders-primary-secret-backup.asc"

gpg --homedir "$VC_GNUPG_HOME" \
  --output "$VC_GPG_BACKUP/vetcoders-primary-revocation.asc" \
  --generate-revocation "$VC_PRIMARY_FPR"
```

Przy generowaniu revocation certificate wybierz ogólny powód, dodaj krótką
notatkę i potwierdź. Powtórz recovery kit na drugim szyfrowanym nośniku, sprawdź
sumy plików, a potem odłącz oba. Przechowuj je w dwóch fizycznie różnych
miejscach.

Plik z sekretnym primary key i revocation certificate są krytycznymi sekretami.
Publiczny `.asc` i fingerprint nie są sekretami.

## 4. Wyeksportuj do CI tylko właściwy podklucz

Dla każdego repo eksportuj wyłącznie przypisany mu podklucz. Wykrzyknik wymusza
dokładnie wskazany klucz:

```sh
VC_PRODUCT_SUBKEY_FPR="1111..."

gpg --homedir "$VC_GNUPG_HOME" --armor \
  --export-secret-subkeys "$VC_PRODUCT_SUBKEY_FPR!" \
  >"vc-frame-ci-signing-subkey.asc"
```

Ten plik nadal jest sekretem. Po wpisaniu do GitHub usuń roboczą kopię
bezpieczną metodą właściwą dla użytego, szyfrowanego nośnika. Nigdy nie
eksportuj pełnego primary secret key do GitHub Actions.

W każdym repo utwórz chronione środowisko GitHub o nazwie `release`, ustaw
wymaganego reviewera i dodaj do niego:

- `GPG_PRIVATE_KEY` — eksport sekretnego podklucza tylko tego produktu;
- `GPG_PASSPHRASE` — hasło klucza;
- `GPG_PUBLIC_KEY` — pełny publiczny klucz Vetcoders;
- opcjonalnie `GPG_SIGNING_KEY_ID` — fingerprint podklucza, jeśli workflow
  wybiera go jawnie.

Każdy job używający prywatnego klucza powinien działać w środowisku `release`.
Publicznego fingerprintu primary key nie chowaj w sekrecie: wpisz go na stałe
do instalatora, dokumentacji i testów kontraktu.

## 5. Opublikuj tożsamość niezależnie od release

Ten sam primary fingerprint i publiczny klucz opublikuj co najmniej w:

- `SECURITY.md` każdego produktu;
- stronie `vetcoders.io/security`;
- GitHub Organization profile albo przypiętym repo bezpieczeństwa;
- WKD dla domeny Vetcoders;
- `keys.openpgp.org` — po wysłaniu klucza potwierdź adres e-mail;
- opisie pierwszego podpisanego GitHub Release.

Nie uznawaj fingerprintu widocznego wyłącznie obok pliku `.sig` za źródło
zaufania. Atakujący, który podmieni oba pliki, poda również własny fingerprint.

## 6. Włącz OIDC trusted publishing per produkt

GPG nie zastępuje logowania do rejestru. Dla każdego kanału skonfiguruj
zaufanego publishera wskazującego dokładne repo, workflow i — jeżeli dostępne —
środowisko `release`:

- GitHub Releases: `actions/attest` oraz `gh attestation verify`;
- PyPI: Trusted Publisher i `pypa/gh-action-pypi-publish`;
- npm: Trusted Publisher, aktualny npm/Node i GitHub-hosted runner;
- crates.io: Trusted Publishing dla dokładnego workflow.

Po przejściu na OIDC usuń stałe tokeny publikujące. Nie dodawaj tokenu
„awaryjnego” bez konkretnej, udokumentowanej procedury break-glass.

## 7. Canary przed pierwszym publicznym release

Na maszynie podpisującej:

```sh
printf 'Vetcoders signing canary\n' >canary.txt
gpg --homedir "$VC_GNUPG_HOME" \
  --local-user "$VC_PRODUCT_SUBKEY_FPR!" \
  --armor --detach-sign canary.txt
```

Na czystej maszynie, która nie ma żadnych kluczy Vetcoders:

```sh
GNUPGHOME="$(mktemp -d)"
chmod 700 "$GNUPGHOME"
export GNUPGHOME

gpg --import vetcoders-release-public.asc
gpg --status-fd 1 --verify canary.txt.asc canary.txt
gpg --with-colons --fingerprint
```

Porównaj pełny primary fingerprint z wartością opublikowaną na stronie
Vetcoders i przypiętą w instalatorze. Potem uruchom candidate release i sprawdź
na czystej maszynie:

1. podpis GPG każdego artefaktu;
2. checksum;
3. GitHub attestation;
4. wynik `--version` i `--build-info`;
5. prawdziwy install/uninstall.

Pierwszy release jest gotowy dopiero wtedy, gdy ta ścieżka przejdzie bez
ręcznych wyjątków.

## Tokeny sprzętowe: ważne ograniczenie

Typowy token OpenPGP ma jeden slot podpisujący. Nie pomieści równocześnie
oddzielnych podkluczy podpisujących vc-frame, CodeScribe i Pensieve. Są trzy
uczciwe opcje:

1. osobny token i zapasowy token dla każdego produktu;
2. tokeny tylko dla podpisów ręcznych, a osobne podklucze produktów w
   chronionych środowiskach CI;
3. jeden wspólny podklucz sprzętowy — prostszy, ale z większym blast radiusem.

Dla Vetcoders na dziś rekomendowana jest opcja 2: offline primary key, osobne
roczne podklucze produktów w chronionym CI, dwa zaszyfrowane recovery kity i
dwa tokeny do operacji ręcznych. Nie kupuj trzech par tokenów, dopóki realny
kanał release nie uzasadnia tego kosztu.

## Rotacja i incydent

Co najmniej 30 dni przed wygaśnięciem:

1. na offline primary key utwórz nowy podklucz produktu;
2. zaktualizuj chronione sekrety środowiska `release`;
3. wykonaj canary oraz candidate release;
4. opublikuj nowy publiczny klucz;
5. dopiero potem pozwól staremu podkluczowi wygasnąć.

Jeżeli wycieknie podklucz jednego produktu, odwołaj tylko ten podklucz, opublikuj
zaktualizowany publiczny klucz i wydaj czysty patch release. Jeżeli wycieknie
primary key, użyj offline revocation certificate, zatrzymaj wszystkie release'y
i przeprowadź nową ceremonię korzenia organizacji.

## Checklista gotowości Vetcoders

- [ ] primary key jest `cert`-only i pozostaje offline;
- [ ] istnieją osobne podklucze vc-frame, CodeScribe i Pensieve;
- [ ] dwa recovery kity zostały sprawdzone i rozdzielone fizycznie;
- [ ] revocation certificate da się odczytać;
- [ ] CI otrzymało tylko właściwy secret subkey;
- [ ] środowisko `release` wymaga reviewera;
- [ ] primary fingerprint jest przypięty w instalatorze;
- [ ] publiczny klucz i fingerprint są dostępne przez niezależne kanały;
- [ ] registry publishing używa OIDC zamiast stałych tokenów;
- [ ] canary przechodzi na czystej maszynie;
- [ ] candidate release przechodzi pełny install/verify/uninstall.

