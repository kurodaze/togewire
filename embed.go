package togewire

import "embed"

//go:embed web/templates/* web/static/css/* web/static/js/*
var EmbeddedFiles embed.FS
