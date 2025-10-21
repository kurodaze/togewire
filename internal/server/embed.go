package server

import (
	"embed"
	"io/fs"
)

// EmbeddedFiles will be set from main package
var EmbeddedFiles *EmbedWrapper

type EmbedWrapper struct {
	FS    embed.FS
	IsSet bool
}

func SetEmbeddedFiles(files embed.FS) {
	EmbeddedFiles = &EmbedWrapper{
		FS:    files,
		IsSet: true,
	}
}

func GetSubFS(path string) (fs.FS, error) {
	if EmbeddedFiles != nil && EmbeddedFiles.IsSet {
		return fs.Sub(EmbeddedFiles.FS, path)
	}
	return nil, nil
}
