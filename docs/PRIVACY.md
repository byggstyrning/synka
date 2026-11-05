# Synka privacy policy

> **Draft.** Not yet reviewed by counsel. The organisation number below is still to be filled in.

Last updated: 4 November 2026

Synka is a Windows desktop app published by Byggstyrning (https://byggstyrning.se). It syncs
construction documents from Autodesk Construction Cloud (ACC Docs), StreamBIM and Interaxo to
an ordinary folder in Windows Explorer. The ACC-only edition of Synka syncs ACC Docs alone. This
policy describes what data Synka handles, where it goes and how you remove it.

**In short:** Synka runs entirely on your PC. Your files and account data move only between your
PC and the platform they belong to. Byggstyrning receives nothing: Synka has no telemetry, no
analytics, no crash reporting and no advertising.

## Who we are

Byggstyrning i Mälardalen AB, organisation number `[to be supplied]`, Sweden
(https://byggstyrning.se, info@byggstyrning.se), publishes Synka. We are the controller for the
personal data we handle ourselves: the messages you send us and our replies. We do not receive
the data Synka handles on your PC (below).

## What Synka handles, and where it is kept

| Data | Why | Where it is kept |
|---|---|---|
| Your account identity on each platform (user ID, name, e-mail address) | To show who is signed in and to label your own changes | On your PC, in Synka's app data folder |
| Sign-in tokens | To stay signed in without asking for your password again | On your PC. Long-lived tokens are encrypted with Windows DPAPI for your Windows user only; short-lived access tokens are kept in memory only |
| Project, folder and file metadata (names, IDs, versions, sizes, dates, who last changed a file, lock state) | To show your projects in Explorer and to know what changed | On your PC, in one local database file |
| File contents | The files you sync | In your sync folder, which you choose (`%USERPROFILE%\Synka` by default) |
| Settings | Your choices in Synka | On your PC, in `settings.json` |
| Logs (technical events and errors, never file contents) | Troubleshooting | On your PC; one file per day, the oldest is deleted after seven files |

Synka never stores the password you use with Autodesk or Interaxo; you sign in on the platform's
own web page. A StreamBIM password is sent once to StreamBIM to sign you in and is never stored
or logged.

## Who receives data

Synka sends your files and account data only between your PC and the platform they belong to:

- **Autodesk** (the Autodesk Platform Services endpoints, its sign-in and Autodesk's file
  storage) for ACC projects. Autodesk's own [privacy statement](https://www.autodesk.com/company/legal-notices-trademarks/privacy-statement)
  applies to that data.
- **StreamBIM** (the servers of the region you sign in to, the region list and the S3 storage
  they hand out links for) for StreamBIM projects.
- **Interaxo** (its sign-in service and API hosts) for Interaxo rooms.

The ACC-only edition contacts Autodesk alone. Synka has no telemetry and does not send data to
Byggstyrning or to any other third party. Synka does not check for updates unless your
organisation has turned that on.

Synka asks Autodesk only for the access it needs: reading and writing project files and reading
your basic profile (`data:read data:write data:create user-profile:read`). Your existing ACC
project permissions still decide what you can see and change. An ACC account administrator must
approve Synka for each ACC account before it can reach that account's projects.

## How long data is kept, and how to remove it

- **Sign out** in Synka (the Konton page) to delete the stored tokens and account identity for
  that account from your PC. For Interaxo, Synka also asks Interaxo to revoke the token.
- **Uninstall** Synka to delete its app data folder (database, settings, tokens and logs).
  Files in your sync folder are your own files and stay where they are until you delete them.
- **Revoke access** at any time in your Autodesk account settings (https://accounts.autodesk.com),
  or ask your ACC account administrator to remove Synka from the account.

Because Byggstyrning never receives the data Synka handles, we hold none of it that we could
export or delete. If you have a question or a request, write to us and we will help.

## Legal basis

The data Synka handles stays on your PC and with the platform it belongs to; the platform's own
terms and privacy statement apply there. When you write to us, we handle your name, e-mail
address and message to answer you, on the basis of our legitimate interest in giving support
(Article 6(1)(f) GDPR). We keep that correspondence only as long as it is needed for the matter.

## Your rights

You have the right to ask us for access to the personal data we hold about you, to have it
corrected or erased, and to restrict or object to our handling of it; where the law gives it,
also to receive it in a portable format. Write to info@byggstyrning.se. You can also complain to
the Swedish Authority for Privacy Protection (Integritetsskyddsmyndigheten, IMY),
https://www.imy.se.

## Children

Synka is a professional tool for the construction industry and is not directed to anyone under 18.

## Changes

We update this policy when Synka's data handling changes. The date at the top shows the latest
version.

## Contact

Byggstyrning i Mälardalen AB, https://byggstyrning.se, info@byggstyrning.se
