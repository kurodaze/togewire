package server

import "embed"

var (
	embeddedFS  embed.FS
	embeddedSet bool
)

func SetEmbeddedFiles(files embed.FS) {
	embeddedFS = files
	embeddedSet = true
}

func getEmbeddedFiles() (embed.FS, bool) {
	return embeddedFS, embeddedSet
}
