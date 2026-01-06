# music-backup

By following these steps you will be able to schedule a background task that backup 
all files in a folder (i.e., your music files) to Google Cloud Storage

Backups will be in a "bucket" named "music-backup". The backups will be organized by 
their day like `02012026\0.zip`, `02012026\1.zip` and so on where every 25 files are 
zipped together. There will also be a `02012026\manifest.json` that lists the specific 
files included in each specific `.zip` such that you can backup in smaller chunks if you
want something you accidentally deleted back. 

Also doing it in smaller files makes it less likely to slow down whatever you have running.
With that in mind and since you may have a lot of files I wrote it in rust so it should be
relatively fast/lightweight.


## Insutrctions for Windows
1. Start by making a Google Cloud account [here].
