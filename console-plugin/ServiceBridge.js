.pragma library

var sharedService = null
var nextRootOrdinalValue = 0

function allocateRootOrdinal() {
  nextRootOrdinalValue += 1
  return nextRootOrdinalValue
}

function publish(service) {
  sharedService = service
}

function clear(service) {
  if (sharedService === service) sharedService = null
}

function current() {
  return sharedService
}
