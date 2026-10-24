<#
.SYNOPSIS
    Collects read-only driver information for connected TASCAM US-HR interfaces.

.DESCRIPTION
    Lists the Plug and Play devices for TASCAM vendor ID 0644, the drivers bound
    to them, their registered device interfaces, and installed TASCAM driver
    packages. The script only reads device properties and the registry. It does
    not install, remove, or rebind drivers and sends nothing to the device.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File collect-driver-info.ps1 > us-hr-driver-info.txt
#>

[CmdletBinding()]
param()

$ErrorActionPreference = 'Continue'

$vendorPattern = 'VID_0644&PID_(806F|8070|8071)'
$propertyKeys = @(
    'DEVPKEY_Device_Service',
    'DEVPKEY_Device_DriverProvider',
    'DEVPKEY_Device_DriverVersion',
    'DEVPKEY_Device_DriverDate',
    'DEVPKEY_Device_DriverInfPath',
    'DEVPKEY_Device_HardwareIds',
    'DEVPKEY_Device_CompatibleIds',
    'DEVPKEY_Device_Parent',
    'DEVPKEY_Device_Children'
)

function Write-Section([string] $Title) {
    Write-Output ''
    Write-Output "== $Title =="
}

Write-Section 'System'
Write-Output ("Windows: {0} build {1}" -f [Environment]::OSVersion.VersionString,
    (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion').CurrentBuild)

Write-Section 'TASCAM US-HR Plug and Play devices'
$devices = @(Get-PnpDevice -PresentOnly | Where-Object { $_.InstanceId -match $vendorPattern })
if ($devices.Count -eq 0) {
    Write-Output 'No US-HR device is connected.'
}
foreach ($device in $devices) {
    Write-Output ''
    Write-Output ("{0}" -f $device.InstanceId)
    Write-Output ("  Name:   {0}" -f $device.FriendlyName)
    Write-Output ("  Class:  {0}" -f $device.Class)
    Write-Output ("  Status: {0}" -f $device.Status)
    foreach ($key in $propertyKeys) {
        $property = Get-PnpDeviceProperty -InstanceId $device.InstanceId -KeyName $key -ErrorAction SilentlyContinue
        if ($null -ne $property -and $null -ne $property.Data) {
            Write-Output ("  {0}: {1}" -f ($key -replace '^DEVPKEY_Device_', ''), (@($property.Data) -join '; '))
        }
    }
}

Write-Section 'Registered device interfaces'
$deviceClasses = 'HKLM:\SYSTEM\CurrentControlSet\Control\DeviceClasses'
Get-ChildItem $deviceClasses -ErrorAction SilentlyContinue | ForEach-Object {
    $interfaceClass = $_.PSChildName
    Get-ChildItem $_.PSPath -ErrorAction SilentlyContinue |
        Where-Object { $_.PSChildName -match $vendorPattern } |
        ForEach-Object {
            $instance = (Get-ItemProperty $_.PSPath -ErrorAction SilentlyContinue).DeviceInstance
            Write-Output ("{0}  {1}  (device {2})" -f $interfaceClass, $_.PSChildName, $instance)
        }
}

Write-Section 'Installed TASCAM or TEAC driver packages'
$driverPackages = @(Get-CimInstance Win32_PnPSignedDriver -ErrorAction SilentlyContinue |
    Where-Object { $_.DeviceID -match $vendorPattern -or $_.Manufacturer -match 'TASCAM|TEAC' })
foreach ($package in $driverPackages) {
    Write-Output ("{0} | {1} | {2} {3} | {4}" -f $package.DeviceName, $package.DriverProviderName,
        $package.DriverVersion, $package.DriverDate, $package.InfName)
}
if ($driverPackages.Count -eq 0) {
    Write-Output 'No signed driver record names TASCAM, TEAC, or a US-HR device.'
}

Write-Section 'Installed TASCAM software'
$programFolders = @($env:ProgramFiles, ${env:ProgramFiles(x86)}, $env:ProgramData) |
    Where-Object { $_ } |
    ForEach-Object { Join-Path $_ 'TASCAM' } |
    Where-Object { Test-Path $_ }
foreach ($folder in $programFolders) {
    Get-ChildItem $folder -Recurse -Include *.exe, *.dll, *.sys -ErrorAction SilentlyContinue |
        ForEach-Object {
            Write-Output ("{0}  {1}" -f $_.FullName, $_.VersionInfo.FileVersion)
        }
}
if (@($programFolders).Count -eq 0) {
    Write-Output 'No TASCAM folder under Program Files or ProgramData.'
}
