param([Parameter(Mandatory=$true)][string]$Image)
$ErrorActionPreference='Stop'
$root=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\target\validation')).TrimEnd('\')
$path=[IO.Path]::GetFullPath($Image)
if(-not $path.StartsWith($root+'\',[StringComparison]::OrdinalIgnoreCase)) { throw 'Not a validation image' }
$disk=Get-DiskImage -ImagePath $path | Get-Disk
$partition=Get-Partition -DriveLetter R
if($disk.Number -ne $partition.DiskNumber) { throw 'R: is not this validation image' }
Dismount-DiskImage -ImagePath $path | Out-Null
