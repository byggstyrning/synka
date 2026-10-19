Synka
=====

Synkar ACC Docs-filer till en vanlig mapp på datorn. Kan köras bredvid Autodesk
Desktop Connector. Allt synkas via internet till din egen mapp; inget annat
program behöver köras.


SÅ HÄR INSTALLERAR DU
---------------------

1. Packa upp HELA zip-filen till en mapp. Kör inte installationen direkt från
   inuti zip-visaren: installationsprogrammet behöver alla filer bredvid varandra.

2. Dubbelklicka på  Install.cmd

3. Windows frågar efter administratörsrättigheter. Svara Ja.

Installationen startar Synka när den är klar. Appen heter "Synka" i Start-menyn.

Har du Better Desktop Connector installerad tar installationen bort den först
(efter en fråga). Se "BYTE FRÅN BETTER DESKTOP CONNECTOR" nedan.


VARFÖR FRÅGAR DEN EFTER ADMINISTRATÖR?
--------------------------------------

Appen är signerad med ett självsignerat certifikat, inte ett köpt. Windows
installerar inte ett paket vars signatur den inte känner igen, så certifikatet
måste litas först - och det kräver administratörsrätt. Det görs en gång per dator.

Installationen gör tre saker:
  1. Litar certifikatet (kräver administratör, en gång per dator)
  2. Installerar Microsoft.WindowsAppRuntime, ramverket appen använder (ingår i zip-filen)
  3. Installerar Synka för dig, som vanligt användarkonto, och startar den

Ingenting hämtas från internet under installationen. Allt som behövs finns i zip-filen.


EFTER INSTALLATIONEN
--------------------

  Start         Synka startar automatiskt när du loggar in. Den ligger som
                en ikon i aktivitetsfältet (nere till höger, bredvid klockan).
                Du kan stänga av autostarten under Inställningar > Appar > Start.

  Synkade filer Synka lägger dina ACC Docs-filer i mappen
                %USERPROFILE%\Synka, det vill säga C:\Users\<ditt namn>\Synka.
                Filerna syns där i Utforskaren. Filer som inte laddats ner än
                visas som platshållare och hämtas när du öppnar dem.

  Desktop       Synka kan köras vid sidan av Autodesk Desktop Connector.
  Connector     Du behöver inte avinstallera den. De två använder separata
                mappar, så de krockar inte.

  Inloggning    Första gången du öppnar Synka loggar du in med ditt
                Autodesk-konto i webbläsaren.

  Uppdateringar Nya versioner delas via den interna OneDrive-mappen Synka. Packa
                upp den nya zip-filen och kör Install.cmd; dina inställningar
                behålls. Appen söker inte efter uppdateringar själv.


BYTE FRÅN BETTER DESKTOP CONNECTOR
----------------------------------

Har datorn Better Desktop Connector (eller den äldre FastADC-versionen):

  1. Kontrollera i den gamla appen att allt är synkat. Det som inte har synkats
     laddas inte upp av Synka.
  2. Kör Install.cmd. Installationen frågar om den gamla appen ska tas bort.
     Svarar du nej ändras ingenting.
  3. Den gamla mappen %USERPROFILE%\BDC ligger kvar. Ta bort den när du har
     kontrollerat att filerna finns i %USERPROFILE%\Synka.
  4. Logga in igen med ditt Autodesk-konto.
  5. Välj projekten igen.


VIKTIGT
-------

  - Synka är inte en Autodesk-produkt. Autodesks support kan inte hjälpa till med Synka.
  - Administratören för varje ACC-konto (hub) som ska synkas måste godkänna
    Synkas Client ID som Custom Integration, även för hubbar som ägs av en
    beställare eller annan organisation.
  - ACC är huvudkopian. Synka kopierar filer mellan ACC och den här datorn, och
    data kan gå förlorad. Kontrollera viktiga ändringar i ACC.
  - Du omfattas fortfarande av Autodesks egna villkor för ACC och ditt Autodesk-konto.
  - Filinnehåll skickas bara mellan den här datorn och Autodesk.
  - Synka är avsett för internt bruk av Byggstyrnings personal.


KRAV
----

  Windows       Windows 10 version 2004 (build 19041) eller senare, 64-bitars.
                Windows 11 fungerar.

  Internet      Krävs för att synka med ACC Docs.


OM DET INTE FUNGERAR
--------------------

"Administratörsrätten gavs inte"
    Kör Install.cmd igen och svara Ja på frågan från Windows. Eller högerklicka
    på Install.cmd och välj "Kör som administratör".

"Certifikatet är fortfarande inte betrott"
    Samma sak: kör Install.cmd igen och svara Ja på administratörsfrågan.

"Ramverket används av ett program som kör just nu"
    Stäng öppna program och kör Install.cmd igen.

"Ingen Synka_*.msix hittades" eller "Inget certifikat hittades"
    Du kör troligen från inuti zip-visaren. Packa upp HELA zip-filen först.

Windows SmartScreen kan varna för att filen är okänd. Det beror på att certifikatet
inte är köpt från en auktoritet som Microsoft känner till, inte på att något är fel
med filen. Välj "Mer information" och sedan "Kör ändå".


AVINSTALLERA
------------

Inställningar > Appar > Synka > Avinstallera. Vanlig avinstallation.
Dina inställningar i appen tas bort med appen. Kontrollera mappen
%USERPROFILE%\Synka efter avinstallationen om du vill behålla filer som ligger där.

Att köra Install.cmd igen är något annat: då behålls dina inställningar.


Varumärken
----------

Autodesk, Autodesk Construction Cloud, Autodesk Docs, Autodesk Forma och Revit är
registrerade varumärken eller varumärken som tillhör Autodesk, Inc. och/eller dess
dotter- eller närstående bolag. Synka är en fristående produkt från Byggstyrning
och är inte ansluten till, godkänd av eller sponsrad av Autodesk.
