param([Parameter(Mandatory=$true)][string]$Image,[Parameter(Mandatory=$true)][ValidateSet('R','S')][string]$Letter,[string]$Label='FRVALID')
$ErrorActionPreference='Stop'
$root=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\target\validation')).TrimEnd('\')
$path=[IO.Path]::GetFullPath($Image)
if(-not $path.StartsWith($root+'\',[StringComparison]::OrdinalIgnoreCase)) { throw 'Not a validation image' }
if((Get-Item -LiteralPath $path).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Image is a reparse point' }
$disk=Get-DiskImage -ImagePath $path -ErrorAction Stop | Get-Disk -ErrorAction Stop
$partition=Get-Partition -DriveLetter $Letter -ErrorAction Stop
if($disk.Number -ne $partition.DiskNumber -or (Get-Volume -DriveLetter $Letter).FileSystemLabel -ne $Label) { throw 'Drive does not belong to the disposable fixture' }
